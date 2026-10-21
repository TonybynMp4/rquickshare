use std::time::Duration;

use anyhow::anyhow;
use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
use btleplug::platform::{Adapter, Manager};
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::{uuid, Uuid};

const SERVICE_UUID_SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");
// 0xFE2C is Google's Fast Pair UUID, also carried by Find My Device beacons
// and similar that are always around. A phone that's sharing puts Quick
// Share's model ID 0xFC128E first in it.
const SHARING_PREFIX: [u8; 3] = [0xfc, 0x12, 0x8e];

const INNER_NAME: &str = "BleListener";

// Scan in short bursts rather than continuously. A continuous scan holds the
// radio for the life of the app, which on a combo Wi-Fi/Bluetooth chip starves
// other Bluetooth devices (mice lag). A sharing phone repeats its 0xFE2C
// advertisement several times a second for as long as its share sheet is open,
// so a 5 s window still catches it - the only cost is the "nearby" alert
// arriving up to SCAN_OFF later.
const SCAN_ON: Duration = Duration::from_secs(5);
const SCAN_OFF: Duration = Duration::from_secs(10);

// Don't spam, max one alert per 30s
const ALERT_INTERVAL: Duration = Duration::from_secs(30);

/// Raises "a nearby device is sharing" when a phone opens its share sheet.
///
/// Even duty-cycled, the scan is active (BlueZ discovery offers nothing else)
/// and while it runs the controller can't take reconnections from Bluetooth
/// mice and keyboards. So on Linux it first tries a BlueZ advertisement
/// monitor, which the kernel serves with a passive scan, and only scans when
/// BlueZ doesn't provide one (it's behind bluetoothd's experimental flag). It
/// also stands down entirely while the user has the alert turned off.
pub struct BleListener {
    adapter: Adapter,
    sender: Sender<()>,
    enabled: watch::Receiver<bool>,
    last_alert: Option<Instant>,
}

impl BleListener {
    pub async fn new(
        sender: Sender<()>,
        enabled: watch::Receiver<bool>,
    ) -> Result<Self, anyhow::Error> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        if adapters.is_empty() {
            return Err(anyhow!("no bluetooth adapter"));
        }

        Ok(Self {
            adapter: adapters[0].clone(),
            sender,
            enabled,
            last_alert: None,
        })
    }

    pub async fn run(mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        loop {
            tokio::select! {
                // Both are ready after a cancelled listen; stop rather than
                // start listening again.
                biased;
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                on = self.enabled.wait_for(|on| *on) => {
                    // The sender lives in RQS, so this only fails on shutdown.
                    if on.is_err() {
                        break;
                    }
                }
            }

            #[cfg(target_os = "linux")]
            match self.monitor(&ctk).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => warn!("{INNER_NAME}: advertisement monitor failed: {e}"),
            }

            self.scan(&ctk).await?;
        }

        Ok(())
    }

    /// Listen through a BlueZ advertisement monitor until cancelled or turned
    /// off. Returns `Ok(false)` when BlueZ can't monitor, so the caller scans.
    #[cfg(target_os = "linux")]
    async fn monitor(&mut self, ctk: &CancellationToken) -> Result<bool, anyhow::Error> {
        use bluer::monitor::{Monitor, MonitorEvent, Pattern};

        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        // Fails unless bluetoothd runs with experimental features
        // (`Experimental = true` in /etc/bluetooth/main.conf).
        let manager = match adapter.monitor().await {
            Ok(manager) => manager,
            Err(e) => {
                info!("{INNER_NAME}: no advertisement monitor ({e}), falling back to scanning");
                return Ok(false);
            }
        };
        // Declared after `manager` so it drops first, as bluer requires.
        let mut handle = match manager
            .register(Monitor {
                // AD type 0x16 is "Service Data - 16-bit UUID"; the UUID comes
                // first, little-endian, then the data.
                patterns: Some(vec![Pattern::new(
                    0x16,
                    0,
                    &[&[0x2c, 0xfe][..], &SHARING_PREFIX].concat(),
                )]),
                // Found on the first advert from any phone in range, lost after
                // 10 s of silence, so each share sheet opening counts once.
                rssi_high_threshold: Some(-100),
                rssi_high_timeout: Some(Duration::from_secs(1)),
                rssi_low_threshold: Some(-127),
                rssi_low_timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            })
            .await
        {
            Ok(handle) => handle,
            Err(e) => {
                warn!("{INNER_NAME}: BlueZ rejected the advertisement monitor ({e}), falling back to scanning");
                return Ok(false);
            }
        };
        info!("{INNER_NAME}: listening with a passive advertisement monitor");

        // BlueZ reports a device once when it comes in range, not on every
        // advert, so an alert held back by the rate limit never gets a second
        // chance. Track who's still sharing and retry when the limit lifts.
        let mut sharing = std::collections::HashSet::new();
        let mut held_back = false;
        let retry = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(retry);

        loop {
            tokio::select! {
                _ = ctk.cancelled() => return Ok(true),
                _ = turned_off(&mut self.enabled) => {
                    info!("{INNER_NAME}: turned off, releasing the advertisement monitor");
                    return Ok(true);
                }
                _ = &mut retry, if held_back => {
                    held_back = false;
                    if let Some(&device) = sharing.iter().next() {
                        self.alert(device);
                    }
                }
                event = handle.next() => match event {
                    Some(MonitorEvent::DeviceFound(id)) => {
                        trace!("{INNER_NAME}: monitor found {}", id.device);
                        sharing.insert(id.device);
                        if !self.alert(id.device) {
                            held_back = true;
                            retry.as_mut().reset(self.next_alert());
                        }
                    }
                    Some(MonitorEvent::DeviceLost(id)) => {
                        trace!("{INNER_NAME}: monitor lost {}", id.device);
                        sharing.remove(&id.device);
                    }
                    Some(_) => {}
                    None => {
                        warn!("{INNER_NAME}: BlueZ released the advertisement monitor, falling back to scanning");
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Listen with a duty-cycled scan until cancelled or turned off.
    async fn scan(&mut self, ctk: &CancellationToken) -> Result<(), anyhow::Error> {
        let mut events = self.adapter.events().await?;
        // Unfiltered. A sharing phone carries 0xFE2C only as service data, not
        // in its service UUID list, and BlueZ's UUID filter matches the list -
        // so a filtered scan never heard the phone unless some other client
        // happened to be scanning unfiltered. The check below does the
        // filtering instead. BlueZ filters on the host, so this costs no airtime.
        let filter = ScanFilter::default();
        self.adapter.start_scan(filter.clone()).await?;
        info!("{INNER_NAME}: listening with a duty-cycled scan");

        let mut scanning = true;
        let toggle = tokio::time::sleep(SCAN_ON);
        tokio::pin!(toggle);

        loop {
            tokio::select! {
                _ = ctk.cancelled() => break,
                _ = turned_off(&mut self.enabled) => {
                    info!("{INNER_NAME}: turned off, stopping the scan");
                    break;
                }
                _ = &mut toggle => {
                    if scanning {
                        if let Err(e) = self.adapter.stop_scan().await {
                            warn!("{INNER_NAME}: could not pause scanning: {e}");
                        }
                        toggle.as_mut().reset(Instant::now() + SCAN_OFF);
                    } else {
                        if let Err(e) = self.adapter.start_scan(filter.clone()).await {
                            warn!("{INNER_NAME}: could not resume scanning: {e}");
                        }
                        toggle.as_mut().reset(Instant::now() + SCAN_ON);
                    }
                    scanning = !scanning;
                }
                Some(e) = events.next() => {
                    // The scan is unfiltered, so this is the only filter.
                    // See also https://github.com/Martichou/rquickshare/issues/74
                    if let CentralEvent::ServiceDataAdvertisement { id, service_data } = e {
                        if service_data
                            .get(&SERVICE_UUID_SHARING)
                            .is_some_and(|data| data.starts_with(&SHARING_PREFIX))
                        {
                            let _ = self.alert(id);
                        }
                    }
                }
            }
        }

        if scanning {
            let _ = self.adapter.stop_scan().await;
        }

        Ok(())
    }

    /// Alert unless one went out within ALERT_INTERVAL. Returns whether it did.
    fn alert(&mut self, who: impl std::fmt::Display) -> bool {
        if self
            .last_alert
            .is_some_and(|last| last.elapsed() < ALERT_INTERVAL)
        {
            return false;
        }

        debug!("{INNER_NAME}: A device ({who}) is sharing nearby");
        // Errs only when nobody listens, and then there's no one to tell.
        let _ = self.sender.send(());
        self.last_alert = Some(Instant::now());
        true
    }

    fn next_alert(&self) -> Instant {
        self.last_alert
            .map_or_else(Instant::now, |last| last + ALERT_INTERVAL)
    }
}

/// Resolves once the alert is turned off.
async fn turned_off(enabled: &mut watch::Receiver<bool>) {
    if enabled.wait_for(|on| !*on).await.is_err() {
        // The sender lives in RQS, so this only happens on shutdown, which
        // the cancellation token already covers.
        std::future::pending::<()>().await;
    }
}
