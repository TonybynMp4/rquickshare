use std::time::{Duration, SystemTime};

use anyhow::anyhow;
use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter};
use btleplug::platform::{Adapter, Manager};
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;
use uuid::{uuid, Uuid};

const SERVICE_UUID_SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");

const INNER_NAME: &str = "BleListener";

// Scan in short bursts rather than continuously. A continuous scan holds the
// radio for the life of the app, which on a combo Wi-Fi/Bluetooth chip starves
// other Bluetooth devices (mice lag). A sharing phone repeats its 0xFE2C
// advertisement several times a second for as long as its share sheet is open,
// so a 5 s window still catches it - the only cost is the "nearby" alert
// arriving up to SCAN_OFF later.
const SCAN_ON: Duration = Duration::from_secs(5);
const SCAN_OFF: Duration = Duration::from_secs(10);

pub struct BleListener {
    adapter: Adapter,
    sender: Sender<()>,
}

impl BleListener {
    pub async fn new(sender: Sender<()>) -> Result<Self, anyhow::Error> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        if adapters.is_empty() {
            return Err(anyhow!("no bluetooth adapter"));
        }

        Ok(Self {
            adapter: adapters[0].clone(),
            sender,
        })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        let mut events = self.adapter.events().await?;
        // Filter on the NearyShare/QuickShare services UUID
        let filter = ScanFilter {
            services: vec![SERVICE_UUID_SHARING],
        };
        self.adapter.start_scan(filter.clone()).await?;

        let mut last_alert = SystemTime::UNIX_EPOCH;
        let mut scanning = true;
        let toggle = tokio::time::sleep(SCAN_ON);
        tokio::pin!(toggle);

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                _ = &mut toggle => {
                    if scanning {
                        if let Err(e) = self.adapter.stop_scan().await {
                            warn!("{INNER_NAME}: could not pause scanning: {e}");
                        }
                        toggle.as_mut().reset(tokio::time::Instant::now() + SCAN_OFF);
                    } else {
                        if let Err(e) = self.adapter.start_scan(filter.clone()).await {
                            warn!("{INNER_NAME}: could not resume scanning: {e}");
                        }
                        toggle.as_mut().reset(tokio::time::Instant::now() + SCAN_ON);
                    }
                    scanning = !scanning;
                }
                Some(e) = events.next() => {
                    match e {
                        CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                            // Sanity check as per: https://github.com/Martichou/rquickshare/issues/74
                            // Seems like the filtering is not enough, so we'll add a check before
                            // proceeding with the service_data.
                            if !service_data.contains_key(&SERVICE_UUID_SHARING) {
                                continue;
                            }

                            let now = SystemTime::now();
                            // Don't spam, max once per 30s
                            if now.duration_since(last_alert)? <= Duration::from_secs(30) {
                                continue;
                            }

                            debug!("{INNER_NAME}: A device ({id}) is sharing ({service_data:?}) nearby");
                            self.sender.send(())?;
                            last_alert = now;
                        },
                        // Not interesting for us
                        _ => {
                            // trace!("{INNER_NAME}: Another CentralEvent got the same services: {:?}", e);
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
}
