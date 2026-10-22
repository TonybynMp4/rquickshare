#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::collections::HashSet;

#[cfg(not(target_os = "windows"))]
use anyhow::anyhow;
#[cfg(not(target_os = "windows"))]
use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
#[cfg(not(target_os = "windows"))]
use btleplug::platform::{Adapter, Manager};
#[cfg(not(target_os = "windows"))]
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio::sync::watch;
#[cfg(not(target_os = "windows"))]
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
#[cfg(not(target_os = "windows"))]
use uuid::{uuid, Uuid};

#[cfg(not(target_os = "windows"))]
const SERVICE_UUID_SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");
// 0xFE2C is Google's Fast Pair UUID, also carried by Find My Device beacons
// and similar that are always around. A phone that's sharing puts Quick
// Share's model ID 0xFC128E first in it.
#[cfg(not(target_os = "windows"))]
const SHARING_PREFIX: [u8; 3] = [0xfc, 0x12, 0x8e];
// The same match for the platform filters, as AD type 0x16 ("Service Data -
// 16-bit UUID") content: the UUID first, little-endian, then the prefix.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const AD_TYPE_SERVICE_DATA_16: u8 = 0x16;
#[cfg(any(target_os = "linux", target_os = "windows"))]
const SHARING_SERVICE_DATA: [u8; 5] = [0x2c, 0xfe, 0xfc, 0x12, 0x8e];

const INNER_NAME: &str = "BleListener";

// How long a phone stays "sharing" after its last advert. Used directly by the
// scan, and as the lost timeout given to the BlueZ monitor and Windows watcher.
const GONE_AFTER_SECS: u64 = 10;

/// Raises "a nearby device is sharing" when a phone opens its share sheet.
///
/// A sharing phone takes a new random address every few seconds, so each
/// address only lives briefly. The alert goes out when the first one appears
/// and not again until every one of them is gone: once per share, not once
/// per address.
///
/// An active scan keeps the controller from taking reconnections from
/// Bluetooth mice and keyboards, so this listens passively where the platform
/// can: a BlueZ advertisement monitor on Linux (behind bluetoothd's
/// experimental flag) and a passive advertisement watcher on Windows. Anywhere
/// else it falls back to a continuous scan. It stands down entirely while the
/// user has the alert turned off.
pub struct BleListener {
    #[cfg(not(target_os = "windows"))]
    adapter: Adapter,
    sender: Sender<()>,
    enabled: watch::Receiver<bool>,
}

impl BleListener {
    pub async fn new(
        sender: Sender<()>,
        enabled: watch::Receiver<bool>,
    ) -> Result<Self, anyhow::Error> {
        #[cfg(not(target_os = "windows"))]
        let adapter = {
            let manager = Manager::new().await?;
            let adapters = manager.adapters().await?;
            adapters
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no bluetooth adapter"))?
        };

        Ok(Self {
            #[cfg(not(target_os = "windows"))]
            adapter,
            sender,
            enabled,
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

            #[cfg(target_os = "windows")]
            self.watch(&ctk).await?;
            #[cfg(not(target_os = "windows"))]
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
                patterns: Some(vec![Pattern::new(
                    AD_TYPE_SERVICE_DATA_16,
                    0,
                    &SHARING_SERVICE_DATA,
                )]),
                // BlueZ reports an address found on its first advert in range
                // and lost after GONE_AFTER_SECS without one.
                rssi_high_threshold: Some(-100),
                rssi_high_timeout: Some(Duration::from_secs(1)),
                rssi_low_threshold: Some(-127),
                rssi_low_timeout: Some(Duration::from_secs(GONE_AFTER_SECS)),
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

        let mut sharing = HashSet::new();
        loop {
            tokio::select! {
                _ = ctk.cancelled() => return Ok(true),
                _ = turned_off(&mut self.enabled) => {
                    info!("{INNER_NAME}: turned off, releasing the advertisement monitor");
                    return Ok(true);
                }
                event = handle.next() => match event {
                    Some(MonitorEvent::DeviceFound(id)) => {
                        trace!("{INNER_NAME}: monitor found {}", id.device);
                        if sharing.is_empty() {
                            self.alert(id.device);
                        }
                        sharing.insert(id.device);
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

    /// Listen with a passive advertisement watcher until cancelled or turned
    /// off. Windows matches the sharing service data itself, and its signal
    /// strength filter reports a phone gone with one last advert at -127 dBm.
    #[cfg(target_os = "windows")]
    async fn watch(&mut self, ctk: &CancellationToken) -> Result<(), anyhow::Error> {
        use windows::core::Interface;
        use windows::Devices::Bluetooth::Advertisement::{
            BluetoothLEAdvertisementBytePattern, BluetoothLEAdvertisementReceivedEventArgs,
            BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementWatcherStoppedEventArgs,
            BluetoothLEScanningMode,
        };
        use windows::Foundation::{IReference, PropertyValue, TimeSpan, TypedEventHandler};
        use windows::Storage::Streams::DataWriter;

        const OUT_OF_RANGE_DBM: i16 = -127;

        enum Event {
            Advert { address: u64, rssi: i16 },
            Stopped(i32),
        }

        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.SetScanningMode(BluetoothLEScanningMode::Passive)?;

        let data = DataWriter::new()?;
        data.WriteBytes(&SHARING_SERVICE_DATA)?;
        let pattern = BluetoothLEAdvertisementBytePattern::Create(
            AD_TYPE_SERVICE_DATA_16,
            0,
            &data.DetachBuffer()?,
        )?;
        watcher
            .AdvertisementFilter()?
            .BytePatterns()?
            .Append(&pattern)?;

        // Like the BlueZ monitor: in range from -100 dBm, gone after
        // GONE_AFTER_SECS without an advert.
        let strength = watcher.SignalStrengthFilter()?;
        strength.SetInRangeThresholdInDBm(
            &PropertyValue::CreateInt16(-100)?.cast::<IReference<i16>>()?,
        )?;
        strength.SetOutOfRangeThresholdInDBm(
            &PropertyValue::CreateInt16(-110)?.cast::<IReference<i16>>()?,
        )?;
        strength.SetOutOfRangeTimeout(
            &PropertyValue::CreateTimeSpan(TimeSpan {
                // In 100 ns units.
                Duration: GONE_AFTER_SECS as i64 * 10_000_000,
            })?
            .cast::<IReference<TimeSpan>>()?,
        )?;

        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let on_advert = tx.clone();
        let received = watcher.Received(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let _ = on_advert.send(Event::Advert {
                    address: args.BluetoothAddress()?,
                    rssi: args.RawSignalStrengthInDBm()?,
                });
            }
            Ok(())
        }))?;
        let stopped = watcher.Stopped(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let _ = tx.send(Event::Stopped(args.Error()?.0));
            }
            Ok(())
        }))?;

        watcher.Start()?;
        info!("{INNER_NAME}: listening with a passive advertisement watcher");

        let mut sharing = HashSet::new();
        let result = loop {
            tokio::select! {
                _ = ctk.cancelled() => break Ok(()),
                _ = turned_off(&mut self.enabled) => {
                    info!("{INNER_NAME}: turned off, stopping the advertisement watcher");
                    break Ok(());
                }
                Some(event) = events.recv() => match event {
                    Event::Advert { address, rssi: OUT_OF_RANGE_DBM } => {
                        trace!("{INNER_NAME}: watcher lost {address:012x}");
                        sharing.remove(&address);
                    }
                    Event::Advert { address, .. } => {
                        if sharing.is_empty() {
                            self.alert(format_args!("{address:012x}"));
                        }
                        sharing.insert(address);
                    }
                    // Bluetooth was turned off, or the radio went away.
                    Event::Stopped(error) => {
                        break Err(anyhow::anyhow!(
                            "advertisement watcher stopped (BluetoothError {error})"
                        ));
                    }
                }
            }
        };

        let _ = watcher.RemoveReceived(received);
        let _ = watcher.RemoveStopped(stopped);
        let _ = watcher.Stop();
        result
    }

    /// Listen with a continuous scan until cancelled or turned off.
    ///
    /// The fallback when nothing passive is available. It's an active scan, so
    /// while it runs Bluetooth mice and keyboards may not reconnect - the cost
    /// the setting's description warns about.
    #[cfg(not(target_os = "windows"))]
    async fn scan(&mut self, ctk: &CancellationToken) -> Result<(), anyhow::Error> {
        let mut events = self.adapter.events().await?;
        // Unfiltered. A sharing phone carries 0xFE2C only as service data, not
        // in its service UUID list, and BlueZ's UUID filter matches the list -
        // so a filtered scan never heard the phone unless some other client
        // happened to be scanning unfiltered. The check below does the
        // filtering instead. BlueZ filters on the host, so this costs no airtime.
        self.adapter.start_scan(ScanFilter::default()).await?;
        info!("{INNER_NAME}: listening with a continuous scan");

        // A scan never reports an address gone, so the share counts as over
        // once no sharing advert has arrived for GONE_AFTER_SECS.
        let mut last_heard: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = ctk.cancelled() => break,
                _ = turned_off(&mut self.enabled) => {
                    info!("{INNER_NAME}: turned off, stopping the scan");
                    break;
                }
                Some(e) = events.next() => {
                    // See also https://github.com/Martichou/rquickshare/issues/74
                    if let CentralEvent::ServiceDataAdvertisement { id, service_data } = e {
                        if service_data
                            .get(&SERVICE_UUID_SHARING)
                            .is_some_and(|data| data.starts_with(&SHARING_PREFIX))
                        {
                            let now = Instant::now();
                            let gone = Duration::from_secs(GONE_AFTER_SECS);
                            if last_heard.is_none_or(|t| now - t >= gone) {
                                self.alert(id);
                            }
                            last_heard = Some(now);
                        }
                    }
                }
            }
        }

        let _ = self.adapter.stop_scan().await;

        Ok(())
    }

    fn alert(&self, who: impl std::fmt::Display) {
        debug!("{INNER_NAME}: A device ({who}) is sharing nearby");
        // Errs only when nobody listens, and then there's no one to tell.
        let _ = self.sender.send(());
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
