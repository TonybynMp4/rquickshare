//! Linux BLE receiver advertiser using BlueZ via `bluer`.
//!
//! Advertises a connectable GATT service under UUID 0xFEF3 (Copresence) carrying the
//! Nearby Connections advertisement header in its service data, and serves the full
//! advertisement (with device name and endpoint ID) over characteristic slot 0.
//! Also hosts the uWeave socket characteristics for BLE transfer and bandwidth upgrade.

use std::sync::atomic::{AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluer::{
    adv::Advertisement,
    gatt::local::{
        Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
        CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, Service,
    },
};
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::{uuid, Uuid};

use super::ble_receiver;
use super::Visibility;
use crate::channel::ChannelMessage;

const INNER_NAME: &str = "BleReceiverAdvertiser";

// Advertise at roughly 1 s instead of the controller's default. The phone scans
// at low latency while its share sheet is open, so it still finds us within a
// second or two, and the radio is left free for other Bluetooth devices.
const ADV_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const ADV_MAX_INTERVAL: Duration = Duration::from_millis(1280);

// Copresence service 0000FEF3-0000-1000-8000-00805F9B34FB.
const COPRESENCE_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");

// Per-slot advertisement characteristic 00000000-0000-3000-8000-000000000000 (slot 0).
// A peer that finds our advertisement connects and reads this to get the full
// BleAdvertisement containing the device name and endpoint info.
const ADV_SLOT0_CHARACTERISTIC_UUID: Uuid = uuid!("00000000-0000-3000-8000-000000000000");

// Nearby's BLE socket characteristics ("MultiplexBleSocketImpl"):
//   client tx = phone -> us   (phone writes)
//   server tx = us -> phone   (we notify/indicate)
const BLE_SOCKET_CLIENT_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000101");
const BLE_SOCKET_SERVER_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000102");

pub struct BleReceiverAdvertiser {
    endpoint_id: [u8; 4],
    device_type: u8,
    name: String,
    sender: tokio::sync::broadcast::Sender<ChannelMessage>,
    visibility: watch::Receiver<Visibility>,
}

impl BleReceiverAdvertiser {
    pub fn new(
        endpoint_id: [u8; 4],
        device_type: u8,
        name: String,
        sender: tokio::sync::broadcast::Sender<ChannelMessage>,
        visibility: watch::Receiver<Visibility>,
    ) -> Self {
        Self {
            endpoint_id,
            device_type,
            name,
            sender,
            visibility,
        }
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        // Leave Bluetooth off if the user turned it off; this used to power the
        // adapter on at startup. Serve once they turn it on.
        if !adapter.is_powered().await? {
            info!("{INNER_NAME}: Bluetooth is off; waiting for it to be turned on");
            tokio::select! {
                _ = ctk.cancelled() => return Ok(()),
                _ = crate::hdl::bluetooth_turned_on(&session) => {}
            }
        }
        info!(
            "{INNER_NAME}: advertising on Bluetooth adapter {} ({})",
            adapter.name(),
            adapter.address().await?
        );

        // The full form is served over GATT; the 17-byte header that points the
        // phone at it goes in the advertisement packet's service data.
        let full_advertisement = ble_receiver::build_full_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );
        let header = ble_receiver::build_advertisement_header(&full_advertisement);

        // Channels and state shared with GATT handlers
        let inbound_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(None));
        let down_slot: Arc<Mutex<Option<mpsc::UnboundedSender<()>>>> = Arc::new(Mutex::new(None));

        // The device the current session belongs to; see `ActivePeer`.
        let active = Arc::new(Mutex::new(ble_receiver::ActivePeer::default()));

        // Signalled when a Weave ConnectionRequest arrives from a phone, with the
        // generation of the session to start.
        let (new_session_tx, mut new_session_rx) = mpsc::unbounded_channel::<u64>();

        // Outbound Weave sender channel currently registered by the active GATT subscriber
        let current_sub_tx: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(None));

        // Packets queued for the phone and not yet sent, counted where they
        // are queued and where they leave; see `Outstanding`.
        let outstanding = ble_receiver::Outstanding::new();

        // The Weave packet size the last ConnectionConfirm agreed, and the
        // largest notification the session's link carries (ATT MTU - 3, 0 until
        // known). Packets are cut to the smaller.
        let agreed_size = Arc::new(AtomicU16::new(509));
        let link_max = Arc::new(AtomicU16::new(0));

        // Reassembly state for multi-packet incoming Weave messages
        struct WeaveRx {
            acc: Vec<u8>,
            expected: u8,
            pending: [Option<Vec<u8>>; 8],
        }
        let rx_buf = Arc::new(Mutex::new(WeaveRx {
            acc: Vec::new(),
            expected: 0,
            pending: Default::default(),
        }));

        // Outbound Weave packet sequence counter (0..7)
        let tx_counter = Arc::new(AtomicU8::new(0));

        let send_pkt: Arc<dyn Fn(Vec<u8>) + Send + Sync> = {
            let current_sub_tx = current_sub_tx.clone();
            let outstanding = outstanding.clone();
            Arc::new(move |pkt: Vec<u8>| {
                outstanding.add(1);
                if let Ok(slot) = current_sub_tx.lock() {
                    if let Some(tx) = slot.as_ref() {
                        if tx.send(pkt).is_err() {
                            warn!("{INNER_NAME}: failed to queue BLE packet; subscriber gone");
                            outstanding.done(1);
                        }
                    } else {
                        warn!("{INNER_NAME}: no active subscriber to send BLE packet to");
                        outstanding.done(1);
                    }
                }
            })
        };

        // --- Build local GATT application ---
        let app = Application {
            services: vec![Service {
                uuid: COPRESENCE_SERVICE_UUID,
                primary: true,
                characteristics: vec![
                    // Slot 0 characteristic: full advertisement
                    Characteristic {
                        uuid: ADV_SLOT0_CHARACTERISTIC_UUID,
                        read: Some(CharacteristicRead {
                            read: true,
                            fun: Box::new({
                                let adv = full_advertisement.clone();
                                move |req| {
                                    let adv = adv.clone();
                                    async move {
                                        let offset = req.offset as usize;
                                        let slice_len = if offset >= adv.len() { 0 } else { adv.len() - offset };
                                        info!(
                                            "*** {INNER_NAME}: served advertisement over GATT read (offset={}, mtu={}, device={}, adv_len={}, returning {} B) ***",
                                            req.offset, req.mtu, req.device_address, adv.len(), slice_len
                                        );
                                        if offset >= adv.len() {
                                            Ok(Vec::new())
                                        } else {
                                            Ok(adv[offset..].to_vec())
                                        }
                                    }
                                    .boxed()
                                }
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    // Server TX: us -> phone (indications / notifications)
                    Characteristic {
                        uuid: BLE_SOCKET_SERVER_TX_UUID,
                        notify: Some(CharacteristicNotify {
                            notify: true,
                            indicate: true,
                            method: CharacteristicNotifyMethod::Fun(Box::new({
                                let current_sub_tx = current_sub_tx.clone();
                                let outstanding = outstanding.clone();
                                let down_slot = down_slot.clone();
                                move |mut notifier| {
                                    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                                    // Kept to tell, when this loop ends, whether the
                                    // slot still holds this loop's sender.
                                    let own_tx = sub_tx.clone();
                                    if let Ok(mut slot) = current_sub_tx.lock() {
                                        *slot = Some(sub_tx);
                                    }
                                    let current_sub_tx = current_sub_tx.clone();
                                    let outstanding = outstanding.clone();
                                    let down_slot = down_slot.clone();
                                    async move {
                                        info!(
                                            "{INNER_NAME}: phone subscribed to Server TX (confirming={})",
                                            notifier.confirming()
                                        );
                                        loop {
                                            tokio::select! {
                                                _ = notifier.stopped() => {
                                                    warn!("{INNER_NAME}: phone unsubscribed - BLE link dropped");
                                                    if let Ok(guard) = down_slot.lock() {
                                                        if let Some(tx) = guard.as_ref() {
                                                            let _ = tx.send(());
                                                        }
                                                    }
                                                    break;
                                                }
                                                Some(pkt) = sub_rx.recv() => {
                                                    let res = notifier.notify(pkt).await;
                                                    outstanding.done(1);
                                                    if let Err(e) = res {
                                                        warn!("{INNER_NAME}: notifier.notify failed: {e}");
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                        // A newer subscriber may already have taken the
                                        // slot; clearing it then cut that one off.
                                        if let Ok(mut slot) = current_sub_tx.lock() {
                                            if slot.as_ref().is_some_and(|tx| tx.same_channel(&own_tx)) {
                                                *slot = None;
                                            }
                                        }
                                        // Packets still queued here will never go out;
                                        // left counted, they made every later upgrade
                                        // sit out the full drain wait.
                                        drop(own_tx);
                                        sub_rx.close();
                                        let mut dropped = 0;
                                        while sub_rx.try_recv().is_ok() {
                                            dropped += 1;
                                        }
                                        outstanding.done(dropped);
                                        info!("{INNER_NAME}: server-tx notification loop ended");
                                    }
                                    .boxed()
                                }
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    // Client TX: phone -> us (writes)
                    Characteristic {
                        uuid: BLE_SOCKET_CLIENT_TX_UUID,
                        write: Some(CharacteristicWrite {
                            write: true,
                            write_without_response: true,
                            method: CharacteristicWriteMethod::Fun(Box::new({
                                let rx_buf = rx_buf.clone();
                                let tx_counter = tx_counter.clone();
                                let send_pkt = send_pkt.clone();
                                let new_session_tx = new_session_tx.clone();
                                let inbound_slot = inbound_slot.clone();
                                let active = active.clone();
                                let adapter = adapter.clone();
                                let agreed_size = agreed_size.clone();
                                let link_max = link_max.clone();
                                move |mut buf, req| {
                                    let rx_buf = rx_buf.clone();
                                    let tx_counter = tx_counter.clone();
                                    let send_pkt = send_pkt.clone();
                                    let new_session_tx = new_session_tx.clone();
                                    let inbound_slot = inbound_slot.clone();
                                    let active = active.clone();
                                    let adapter = adapter.clone();
                                    let agreed_size = agreed_size.clone();
                                    let link_max = link_max.clone();
                                    async move {
                                        if buf.is_empty() {
                                            return Ok(());
                                        }

                                        // Whose write this is. A ConnectionRequest may
                                        // start a session; anything else must come from
                                        // the device whose session it is.
                                        let device = req.device_address.to_string();
                                        let is_request = buf[0] & 0x80 != 0 && buf[0] & 0x0f == 0;
                                        let mut started: Option<u64> = None;
                                        let allowed = if is_request {
                                            let owner = active
                                                .lock()
                                                .ok()
                                                .and_then(|a| a.device().map(str::to_string));
                                            // An owner that is no longer connected has
                                            // lost its link; its session only hasn't
                                            // noticed yet.
                                            let owner_connected = match owner
                                                .and_then(|o| o.parse::<bluer::Address>().ok())
                                            {
                                                Some(address) => match adapter.device(address) {
                                                    Ok(d) => d.is_connected().await.unwrap_or(false),
                                                    Err(_) => false,
                                                },
                                                None => false,
                                            };
                                            if let Ok(mut a) = active.lock() {
                                                started = a.start(&device, owner_connected);
                                            }
                                            started.is_some()
                                        } else {
                                            active.lock().is_ok_and(|a| a.owns(&device))
                                        };
                                        if !allowed {
                                            if is_request {
                                                info!(
                                                    "{INNER_NAME}: ignoring a ConnectionRequest from \
                                                     {device}: another device's session is active"
                                                );
                                            } else {
                                                trace!("{INNER_NAME}: ignoring a write from {device}, not this session's");
                                            }
                                            return Ok(());
                                        }

                                        // The session's link MTU, as BlueZ reports it on
                                        // each of the owner's writes; 23 is the ATT minimum.
                                        if req.mtu >= 23 {
                                            link_max.store(req.mtu - 3, Ordering::Relaxed);
                                        }

                                        // Weave framing: bit 7: 0 = data, 1 = control
                                        if buf[0] & 0x80 == 0 {
                                            let counter = ((buf[0] >> 4) & 0x07) as usize;
                                            let mut ready_msgs = Vec::new();
                                            if let Ok(mut st) = rx_buf.lock() {
                                                st.pending[counter] = Some(std::mem::take(&mut buf));

                                                while let Some(pkt) = {
                                                    let exp = st.expected as usize;
                                                    st.pending[exp].take()
                                                } {
                                                    st.expected = (st.expected + 1) & 0x07;
                                                    let header = pkt[0];
                                                    let first = header & 0x08 != 0;
                                                    let last = header & 0x04 != 0;

                                                    if first {
                                                        st.acc.clear();
                                                    }
                                                    st.acc.extend_from_slice(&pkt[1..]);
                                                    if st.acc.len()
                                                        > super::ble_receiver::MAX_WEAVE_MESSAGE
                                                    {
                                                        warn!(
                                                            "{INNER_NAME}: Weave message passed {} B without ending; dropping it",
                                                            super::ble_receiver::MAX_WEAVE_MESSAGE
                                                        );
                                                        st.acc.clear();
                                                        continue;
                                                    }
                                                    if last {
                                                        let msg = std::mem::take(&mut st.acc);
                                                        ready_msgs.push(msg);
                                                    }
                                                }
                                            }

                                            for msg in ready_msgs {
                                                if let Ok(guard) = inbound_slot.lock() {
                                                    if let Some(tx) = guard.as_ref() {
                                                        let _ = tx.send(msg);
                                                    }
                                                }
                                            }
                                        } else {
                                            // Control packet
                                            if let Ok(mut st) = rx_buf.lock() {
                                                st.expected = (((buf[0] >> 4) & 0x07) + 1) & 0x07;
                                                st.acc.clear();
                                                st.pending = Default::default();
                                            }

                                            let cmd = buf[0] & 0x0f;
                                            // Command 0 = ConnectionRequest: [0x80, ver_min(2), ver_max(2), max_pkt(2)]
                                            if cmd == 0 {
                                                info!("*** {INNER_NAME}: received Weave ConnectionRequest ***");
                                                let ver_max = if buf.len() >= 5 {
                                                    u16::from_be_bytes([buf[3], buf[4]])
                                                } else {
                                                    1
                                                };
                                                let their_max = if buf.len() >= 7 {
                                                    u16::from_be_bytes([buf[5], buf[6]])
                                                } else {
                                                    509
                                                };
                                                let version = ver_max.min(1);
                                                let packet_size = their_max.min(509);
                                                agreed_size.store(packet_size, Ordering::Relaxed);

                                                tx_counter.store(0, Ordering::Relaxed);
                                                if let Ok(mut st) = rx_buf.lock() {
                                                    st.acc.clear();
                                                    st.expected = 1;
                                                    st.pending = Default::default();
                                                }

                                                let ctr = tx_counter.fetch_add(1, Ordering::Relaxed) & 0x07;
                                                let confirm = vec![
                                                    0x80 | (ctr << 4) | 0x01,
                                                    (version >> 8) as u8,
                                                    version as u8,
                                                    (packet_size >> 8) as u8,
                                                    packet_size as u8,
                                                ];
                                                info!(
                                                    "*** {INNER_NAME}: sending ConnectionConfirm (v{version}, {packet_size} B) ***"
                                                );
                                                send_pkt(confirm);
                                                if let Some(generation) = started {
                                                    if new_session_tx.send(generation).is_err() {
                                                        warn!("{INNER_NAME}: the session supervisor is gone");
                                                    }
                                                }
                                            } else {
                                                info!("{INNER_NAME}: weave control cmd={cmd} {buf:02x?}");
                                            }
                                        }

                                        Ok(())
                                    }
                                    .boxed()
                                }
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let app_handle = adapter.serve_gatt_application(app).await?;
        info!("{INNER_NAME}: GATT application registered successfully");

        // --- Advertise connectable 0xFEF3 service ---
        // A legacy BLE advertising packet is capped at 31 bytes total: Flags
        // (3 B) + the 0xFEF3 Service UUID (4 B) + the header as 0xFEF3 service
        // data (4 + 17 B) = 28 B. Anything over 31 would make BlueZ switch to
        // Bluetooth 5 Extended Advertising, which Android's legacy scanner
        // cannot see. The header is what tells the phone's scan that there is a
        // NearbySharing advertisement to read; with the bare UUID alone the
        // phone almost never connects. It then reads the full advertisement
        // (name + endpoint) over GATT slot 0.
        let mut le_advertisement = Advertisement {
            advertisement_type: bluer::adv::Type::Peripheral,
            service_uuids: vec![COPRESENCE_SERVICE_UUID].into_iter().collect(),
            service_data: [(COPRESENCE_SERVICE_UUID, header)].into_iter().collect(),
            discoverable: Some(true),
            min_interval: Some(ADV_MIN_INTERVAL),
            max_interval: Some(ADV_MAX_INTERVAL),
            ..Default::default()
        };
        // Hidden means hidden on BLE too, not only on mDNS. The GATT
        // application stays registered either way; it costs no airtime.
        let mut adv_handle = if *self.visibility.borrow() == Visibility::Invisible {
            info!("{INNER_NAME}: hidden, not advertising until visible");
            None
        } else {
            let h = advertise(&adapter, &mut le_advertisement).await?;
            info!("{INNER_NAME}: legacy connectable GATT service advertising started under 0xFEF3");
            Some(h)
        };

        // --- Supervisor: spawn a fresh Nearby session for each Weave connection ---
        let session_sender = self.sender.clone();
        let session_inbound_slot = inbound_slot.clone();
        let session_down_slot = down_slot.clone();
        let session_tx_counter = tx_counter.clone();
        let session_send_pkt = send_pkt.clone();
        let session_outstanding = outstanding.clone();
        let session_agreed_size = agreed_size.clone();
        let session_link_max = link_max.clone();
        let session_active = active.clone();

        tokio::spawn(async move {
            while let Some(generation) = new_session_rx.recv().await {
                info!("{INNER_NAME}: new Weave connection, starting fresh session");
                let end_active = session_active.clone();

                let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let (down_tx, mut down_rx) = mpsc::unbounded_channel::<()>();
                let (upgrade_tx, mut upgrade_rx) =
                    mpsc::unbounded_channel::<tokio::net::TcpStream>();
                let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<()>();
                let (mut ours, theirs) = tokio::io::duplex(256 * 1024);

                *session_inbound_slot.lock().unwrap() = Some(inbound_tx);
                *session_down_slot.lock().unwrap() = Some(down_tx);

                let msg_sender = session_sender.clone();
                let ui_sender = session_sender.clone();

                // 1. Spawn InboundRequest worker
                tokio::spawn(async move {
                    let _pause = crate::hdl::DiscoveryPause::new();
                    let mut request =
                        super::InboundRequest::new(theirs, "ble".to_string(), msg_sender);
                    request.set_upgrade_sink(upgrade_tx, switch_tx);

                    loop {
                        match request.handle().await {
                            Ok(()) => {}
                            Err(e) => {
                                match e.downcast_ref() {
                                    Some(crate::errors::AppError::NotAnError) => {
                                        info!("{INNER_NAME}: BLE session closed normally");
                                    }
                                    _ => {
                                        warn!(
                                            "{INNER_NAME}: BLE InboundRequest ended in state {:?}: {e}",
                                            request.state.state
                                        );
                                        if request.state.state != crate::hdl::State::Finished {
                                            let _ = ui_sender.send(ChannelMessage {
                                                id: "ble".to_string(),
                                                direction:
                                                    crate::channel::ChannelDirection::LibToFront,
                                                state: Some(crate::hdl::State::Disconnected),
                                                ..Default::default()
                                            });
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }
                    // Free the server for the next device, unless a newer
                    // session already took it.
                    if let Ok(mut active) = end_active.lock() {
                        active.end(generation);
                    }
                });

                // 2. Spawn pump: pipes between duplex and Weave BLE framing
                let pump_tx_counter = session_tx_counter.clone();
                let pump_send_pkt = session_send_pkt.clone();
                let pump_outstanding = session_outstanding.clone();
                let agreed_size = session_agreed_size.clone();
                let link_max = session_link_max.clone();
                // Cut to the agreed size, capped by the link's notification size.
                let packet_size = move || {
                    let link = link_max.load(Ordering::Relaxed);
                    ble_receiver::send_packet_size(
                        agreed_size.load(Ordering::Relaxed),
                        (link > 0).then_some(link),
                    )
                };

                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut upgraded: Option<tokio::net::TcpStream> = None;

                    loop {
                        tokio::select! {
                            Some(msg) = inbound_rx.recv() => {
                                if msg.len() > 3 && msg[..3] == [0xfc, 0x9f, 0x5e] {
                                    let body = &msg[3..];
                                    if ours.write_all(body).await.is_err() {
                                        break;
                                    }
                                } else {
                                    debug!("{INNER_NAME}: multiplex control {:02x?}", &msg[..msg.len().min(24)]);
                                }
                            }
                            n = ours.read_buf(&mut pending) => {
                                match n {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {
                                        while pending.len() >= 4 {
                                            let len = u32::from_be_bytes([
                                                pending[0], pending[1], pending[2], pending[3],
                                            ]) as usize;
                                            if pending.len() < 4 + len {
                                                break;
                                            }
                                            let mut out = Vec::with_capacity(3 + 4 + len);
                                            out.extend_from_slice(&[0xfc, 0x9f, 0x5e]);
                                            out.extend_from_slice(&pending[..4 + len]);
                                            pending.drain(..4 + len);

                                            frame_weave(&out, packet_size(), &pump_tx_counter, &pump_send_pkt);
                                        }
                                    }
                                }
                            }
                            Some(sock) = upgrade_rx.recv() => {
                                info!("{INNER_NAME}: upgraded socket ready, holding for SAFE_TO_CLOSE");
                                upgraded = Some(sock);
                            }
                            Some(_) = switch_rx.recv() => {
                                match upgraded.take() {
                                    Some(mut sock) => {
                                        // Flush remaining bytes from the duplex.
                                        // InboundRequest writes SAFE_TO_CLOSE before it
                                        // signals the switch, so they are already there:
                                        // stop at the first read that would wait.
                                        loop {
                                            let mut buf = [0u8; 8192];
                                            match ours.read(&mut buf).now_or_never() {
                                                Some(Ok(n)) if n > 0 => pending.extend_from_slice(&buf[..n]),
                                                _ => break,
                                            }
                                        }
                                        while pending.len() >= 4 {
                                            let len = u32::from_be_bytes([
                                                pending[0], pending[1], pending[2], pending[3],
                                            ]) as usize;
                                            if pending.len() < 4 + len {
                                                break;
                                            }
                                            let mut out = Vec::with_capacity(3 + 4 + len);
                                            out.extend_from_slice(&[0xfc, 0x9f, 0x5e]);
                                            out.extend_from_slice(&pending[..4 + len]);
                                            pending.drain(..4 + len);
                                            frame_weave(&out, packet_size(), &pump_tx_counter, &pump_send_pkt);
                                        }

                                        // Close the multiplex service channel. It is a
                                        // Weave message like any other, so it goes out
                                        // framed; it used to be sent bare, its first byte
                                        // read as a Weave header.
                                        let disconnect = [
                                            0x00, 0x00, 0x00, 0x08, 0x02, 0x1a, 0x05, 0x0a, 0x03, 0xfc, 0x9f, 0x5e,
                                        ];
                                        frame_weave(&disconnect, packet_size(), &pump_tx_counter, &pump_send_pkt);

                                        // Wait for queued packets to drain. The 3s bound
                                        // is a safety net only; it never picks another
                                        // course.
                                        if tokio::time::timeout(Duration::from_secs(3), pump_outstanding.drained())
                                            .await
                                            .is_err()
                                        {
                                            debug!(
                                                "{INNER_NAME}: {} packet(s) still queued after 3s",
                                                pump_outstanding.count()
                                            );
                                        }

                                        info!("{INNER_NAME}: prior channel released, switching to upgraded socket");

                                        let mut first = [0u8; 8192];
                                        let n = match tokio::time::timeout(Duration::from_secs(45), sock.read(&mut first)).await {
                                            Ok(Ok(0)) => {
                                                warn!("{INNER_NAME}: peer closed upgraded socket without sending");
                                                break;
                                            }
                                            Ok(Ok(n)) => n,
                                            Ok(Err(e)) => {
                                                warn!("{INNER_NAME}: upgraded socket failed: {e}");
                                                break;
                                            }
                                            Err(_) => {
                                                warn!("{INNER_NAME}: nothing on upgraded socket within 45s");
                                                break;
                                            }
                                        };
                                        if ours.write_all(&first[..n]).await.is_err() {
                                            break;
                                        }

                                        match tokio::io::copy_bidirectional(&mut sock, &mut ours).await {
                                            Ok((from_phone, to_phone)) => {
                                                info!("{INNER_NAME}: upgraded socket closed ({from_phone} B in, {to_phone} B out)");
                                            }
                                            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {
                                                info!("{INNER_NAME}: phone closed upgraded socket ({e})");
                                            }
                                            Err(e) => warn!("{INNER_NAME}: upgraded socket copy error: {e}"),
                                        }
                                        break;
                                    }
                                    None => warn!("{INNER_NAME}: asked to switch but no upgraded socket arrived"),
                                }
                            }
                            _ = down_rx.recv() => {
                                warn!("{INNER_NAME}: BLE link dropped, tearing bridge down");
                                break;
                            }
                            else => break,
                        }
                    }
                    info!("{INNER_NAME}: bridge pump exited");
                });
            }
        });

        // --- Keep advertiser running and handle discovery pause ---
        // Advertise unless hidden or sending over BLE, following both as they
        // change. A failed resume, e.g. with Bluetooth off, waits for
        // Bluetooth to turn on, or for either of them to change.
        let mut visibility = self.visibility.clone();
        let mut sends = crate::hdl::ble_sends();
        let mut resume_failed = false;
        loop {
            let sending = *sends.borrow_and_update() > 0;
            let hidden = *visibility.borrow_and_update() == Visibility::Invisible;
            if sending || hidden {
                // Dropping the advertisement does not disconnect a phone
                // already on the GATT server, so hiding mid-transfer is safe.
                if adv_handle.take().is_some() {
                    if sending {
                        info!("{INNER_NAME}: outbound BLE send in progress, pausing advertisement");
                    } else {
                        info!("{INNER_NAME}: hidden, pausing advertisement");
                    }
                }
            } else if adv_handle.is_none() && !resume_failed {
                match advertise(&adapter, &mut le_advertisement).await {
                    Ok(h) => {
                        adv_handle = Some(h);
                        info!("{INNER_NAME}: resumed advertisement");
                    }
                    Err(e) => {
                        warn!("{INNER_NAME}: could not resume advertisement ({e}), waiting for Bluetooth to turn on");
                        resume_failed = true;
                    }
                }
            }

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: cancelled, stopping BLE receiver advertiser");
                    drop(adv_handle);
                    drop(app_handle);
                    break;
                }
                Ok(()) = visibility.changed() => resume_failed = false,
                Ok(()) = sends.changed() => resume_failed = false,
                _ = super::ble::bluetooth_turned_on(&session), if resume_failed => {
                    resume_failed = false;
                }
            }
        }

        Ok(())
    }
}

/// Advertise, falling back to BlueZ's default interval if it refuses ours. An
/// older BlueZ may; advertising at the default beats not advertising. The
/// fallback sticks, so a resume after hiding doesn't retry the refused one.
async fn advertise(
    adapter: &bluer::Adapter,
    advertisement: &mut Advertisement,
) -> bluer::Result<bluer::adv::AdvertisementHandle> {
    let e = match adapter.advertise(advertisement.clone()).await {
        Ok(h) => return Ok(h),
        Err(e) => e,
    };
    // With Bluetooth off everything fails, and blaming the interval then
    // would drop it for the rest of the session.
    if advertisement.min_interval.is_none() || !adapter.is_powered().await.unwrap_or(false) {
        return Err(e);
    }
    warn!("{INNER_NAME}: advertising with a custom interval failed ({e}), using the default");
    advertisement.min_interval = None;
    advertisement.max_interval = None;
    adapter.advertise(advertisement.clone()).await
}

/// Cuts `bytes` into Weave data packets of at most `packet_size` bytes and
/// queues each via `send_pkt`.
fn frame_weave(
    bytes: &[u8],
    packet_size: usize,
    tx_counter: &AtomicU8,
    send_pkt: &Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    for pkt in ble_receiver::weave_packets(bytes, packet_size, tx_counter) {
        send_pkt(pkt);
    }
}
