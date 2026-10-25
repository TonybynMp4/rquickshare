use std::sync::Arc;

use bluer::adv::Advertisement;
use bluer::UuidExt;
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SERVICE_DATA: Bytes = Bytes::from_static(&[
    252, 18, 142, 1, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 191, 45, 91, 160, 225, 216, 117, 36, 202, 0,
]);

const INNER_NAME: &str = "BleAdvertiser";

#[derive(Debug, Clone)]
pub struct BleAdvertiser {
    session: bluer::Session,
    adapter: Arc<bluer::Adapter>,
}

impl BleAdvertiser {
    pub async fn new() -> Result<Self, anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;

        Ok(Self {
            session,
            adapter: Arc::new(adapter),
        })
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        // Bluetooth being off is the user's choice. This used to power the
        // adapter on at every send, overriding it; now it waits to be turned
        // on instead.
        if !self.adapter.is_powered().await? {
            info!("{INNER_NAME}: Bluetooth is off; waiting for it to be turned on");
            tokio::select! {
                _ = ctk.cancelled() => return Ok(()),
                _ = crate::hdl::bluetooth_turned_on(&self.session) => {}
            }
        }

        info!(
            "{INNER_NAME}: advertising on Bluetooth adapter {} with address {}",
            self.adapter.name(),
            self.adapter.address().await?
        );

        let service_uuid = Uuid::from_u16(0xFE2C);
        let handle = self
            .adapter
            .advertise(self.get_advertisment(service_uuid, SERVICE_DATA))
            .await?;
        ctk.cancelled().await;
        info!("{INNER_NAME}: tracker cancelled, returning");
        drop(handle);

        Ok(())
    }

    fn get_advertisment(&self, service_uuid: Uuid, adv_data: Bytes) -> Advertisement {
        Advertisement {
            advertisement_type: bluer::adv::Type::Broadcast,
            service_data: [(service_uuid, adv_data.into())].into(),
            ..Default::default()
        }
    }
}
