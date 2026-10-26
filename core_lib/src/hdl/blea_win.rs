//! Windows implementation of the BLE advertiser.
//!
//! Mirrors the Linux (`bluer`) advertiser in `blea.rs`: it broadcasts the
//! Quick Share / Nearby Share BLE service data so a nearby Android device makes
//! its mDNS service available, letting this machine discover the phone when
//! sending (removing the "put the phone in receive mode first" workaround).
//!
//! Uses the WinRT `BluetoothLEAdvertisementPublisher` API via the `windows`
//! crate. All WinRT calls run on a dedicated blocking thread with an
//! initialized (MTA) COM apartment, since WinRT activation requires an
//! initialized apartment and tokio worker threads are not guaranteed to be.

use std::sync::mpsc;

use tokio_util::sync::CancellationToken;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementPublisher,
    BluetoothLEAdvertisementPublisherStatus,
    BluetoothLEAdvertisementPublisherStatusChangedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

// Same 24-byte service data payload broadcast by the Linux advertiser.
const SERVICE_DATA: [u8; 24] = [
    252, 18, 142, 1, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 191, 45, 91, 160, 225, 216, 117, 36, 202, 0,
];

// GAP AD type 0x16 = "Service Data - 16-bit UUID".
const AD_TYPE_SERVICE_DATA_16BIT: u8 = 0x16;
// Quick Share / Nearby Share 16-bit service UUID (0xFE2C).
const SERVICE_UUID_16: u16 = 0xFE2C;

const INNER_NAME: &str = "BleAdvertiser";

// BluetoothLEAdvertisementPublisherStatus enum values.
fn status_name(s: i32) -> &'static str {
    match s {
        0 => "Created",
        1 => "Waiting",
        2 => "Started",
        3 => "Stopping",
        4 => "Stopped",
        5 => "Aborted",
        _ => "Unknown",
    }
}

/// What the advertising thread waits on: the publisher's own status changes,
/// or the advertiser being cancelled.
enum Event {
    Status(BluetoothLEAdvertisementPublisherStatus),
    Cancelled,
}

/// How one advertising run ended.
enum Ended {
    Cancelled,
    /// Windows aborted the advertisement, typically because Bluetooth went off.
    Aborted,
}

pub struct BleAdvertiser;

impl BleAdvertiser {
    pub async fn new() -> Result<Self, anyhow::Error> {
        // The WinRT objects must be created on the COM-initialized advertising
        // thread, so all construction is deferred to `run`.
        Ok(Self)
    }

    /// Advertise until cancelled. When Windows aborts the advertisement
    /// (Bluetooth turned off), wait for Bluetooth to come back and start again
    /// rather than going quiet for the rest of the session.
    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        loop {
            let (tx, rx) = mpsc::channel::<Event>();

            // Cancellation is async and the publisher's thread blocks, so turn
            // it into one more message on the thread's channel.
            let cancel_tx = tx.clone();
            let cancel = ctk.clone();
            let canceller = tokio::spawn(async move {
                cancel.cancelled().await;
                // The thread may already have ended on its own.
                if cancel_tx.send(Event::Cancelled).is_err() {
                    trace!("{INNER_NAME}: cancelled after the advertisement ended");
                }
            });

            let ended = tokio::task::spawn_blocking(move || advertise(tx, rx)).await?;
            canceller.abort();

            match ended? {
                Ended::Cancelled => return Ok(()),
                Ended::Aborted => {
                    info!(
                        "{INNER_NAME}: advertisement aborted; waiting for Bluetooth to come back"
                    );
                    tokio::select! {
                        _ = ctk.cancelled() => return Ok(()),
                        _ = crate::hdl::bluetooth_turned_on() => {}
                    }
                }
            }
        }
    }
}

/// One advertising run, on a COM-initialized blocking thread. Returns when the
/// advertiser is cancelled or Windows aborts the advertisement.
fn advertise(tx: mpsc::Sender<Event>, rx: mpsc::Receiver<Event>) -> Result<Ended, anyhow::Error> {
    // WinRT activation requires an initialized COM apartment. Join the MTA; if
    // this thread is already initialized it is a harmless no-op.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let publisher = BluetoothLEAdvertisementPublisher::new()?;
    let advertisement = publisher.Advertisement()?;

    // Build the "Service Data - 16-bit UUID" AD structure: the 16-bit UUID in
    // little-endian order followed by the service data payload. This is
    // byte-for-byte equivalent to what the bluer advertiser emits on Linux.
    let writer = DataWriter::new()?;
    writer.WriteByte((SERVICE_UUID_16 & 0xFF) as u8)?;
    writer.WriteByte((SERVICE_UUID_16 >> 8) as u8)?;
    writer.WriteBytes(&SERVICE_DATA)?;
    let buffer = writer.DetachBuffer()?;

    let section = BluetoothLEAdvertisementDataSection::new()?;
    section.SetDataType(AD_TYPE_SERVICE_DATA_16BIT)?;
    section.SetData(&buffer)?;
    advertisement.DataSections()?.Append(&section)?;

    // Status changes come to us instead of being polled. Logged so we can tell
    // whether the advertisement is actually on air (Started) or was rejected
    // by the adapter (Aborted, with the reason).
    let status_token = publisher.StatusChanged(&TypedEventHandler::<
        BluetoothLEAdvertisementPublisher,
        BluetoothLEAdvertisementPublisherStatusChangedEventArgs,
    >::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            let status = args.Status()?;
            let error = args.Error().map(|e| e.0).unwrap_or(-1);
            info!(
                "{INNER_NAME}: publisher status = {} ({}), error {error}",
                status.0,
                status_name(status.0)
            );
            // The receiving end is gone only once this run is over.
            if tx.send(Event::Status(status)).is_err() {
                trace!("{INNER_NAME}: status change after the run ended");
            }
        }
        Ok(())
    }))?;

    publisher.Start()?;
    info!("{INNER_NAME}: advertising via WinRT BluetoothLEAdvertisementPublisher");

    let mut ended = Ended::Cancelled;
    for event in rx.iter() {
        match event {
            Event::Cancelled => {
                info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
                break;
            }
            Event::Status(status) if status == BluetoothLEAdvertisementPublisherStatus::Aborted => {
                ended = Ended::Aborted;
                break;
            }
            Event::Status(_) => {}
        }
    }

    if let Err(e) = publisher.RemoveStatusChanged(status_token) {
        warn!("{INNER_NAME}: could not remove the status handler: {e}");
    }
    if let Err(e) = publisher.Stop() {
        // Already stopped when Windows aborted it.
        debug!("{INNER_NAME}: stop: {e}");
    }

    Ok(ended)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advertises on the machine's real Bluetooth adapter, so it's run by hand:
    /// `cargo test -p rqs_lib advertiser_stops_when_cancelled -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a Bluetooth adapter"]
    async fn advertiser_stops_when_cancelled() {
        let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
        let ctk = CancellationToken::new();
        let advertiser = BleAdvertiser::new().await.unwrap();
        let run = tokio::spawn({
            let ctk = ctk.clone();
            async move { advertiser.run(ctk).await }
        });

        // Long enough on air for the publisher to report Started in the log;
        // what the test checks is the cancel afterwards.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        ctk.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("the advertiser did not stop on cancel")
            .unwrap()
            .unwrap();
    }
}
