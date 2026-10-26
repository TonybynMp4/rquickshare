//! Builder for the Quick Share / Nearby Connections BLE *receiver*
//! advertisement (issue #425), so a phone doing BLE-only discovery lists this
//! machine as a target.
//!
//! Two pieces, following google/nearby's GATT route:
//!  - the **advertisement header**, 17 bytes in the 0xFEF3 service data. It
//!    tells a scanning phone there is a NearbySharing advertisement to read
//!    over GATT, and its hash;
//!  - the **full advertisement**, served from the slot-0 GATT characteristic,
//!    which carries the device name the phone needs to list us.
//!
//! The 0xFEF3 service data used to hold the 26-byte *fast* advertisement
//! instead. A connectable advertisement from Windows has no room for it, so
//! Windows dropped it and only the bare 0xFEF3 UUID went on air. The phone then
//! had nothing telling it to read us, and found us only on the rare scan where
//! it tried a GATT read anyway.
//!
//! Phones still advertise in the fast form, so its parser stays for the send
//! path. Layouts are from google/nearby `connections/implementation/mediums/ble`.

use sha2::{Digest, Sha256};

/// Largest Weave message we reassemble: the biggest frame the stack accepts
/// (5 MiB) plus the 3-byte service hash and 4-byte length in front of it. The
/// GATT server takes writes from any nearby device without pairing, so a peer
/// that never sets the "last" bit must not be able to grow a buffer forever.
pub(crate) const MAX_WEAVE_MESSAGE: usize = 5 * 1024 * 1024 + 7;

/// Which device the current BLE receive session belongs to.
///
/// The GATT server takes writes from any device in range without pairing, and
/// its Weave reassembly state and counters are shared. Any device that wrote a
/// ConnectionRequest used to reset them and take over, killing a transfer in
/// progress - even a second phone only looking for Quick Share targets. So a
/// session belongs to the device that opened it, from its ConnectionRequest
/// until its InboundRequest ends, and writes from any other device are
/// ignored. The one exception is an owner that is no longer subscribed: its
/// link is gone and the session is only waiting to notice.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
#[derive(Default)]
pub(crate) struct ActivePeer {
    owner: Option<(u64, String)>,
    next_generation: u64,
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
impl ActivePeer {
    /// Whether writes from `device` belong to the current session.
    pub(crate) fn owns(&self, device: &str) -> bool {
        self.owner.as_ref().is_some_and(|(_, d)| d == device)
    }

    /// Start a session for `device` if it may have one, returning the new
    /// session's generation. `owner_subscribed` says whether the current
    /// owner, if any, is still subscribed.
    pub(crate) fn start(&mut self, device: &str, owner_subscribed: bool) -> Option<u64> {
        if let Some((_, owner)) = &self.owner {
            if owner != device && owner_subscribed {
                return None;
            }
        }
        self.next_generation += 1;
        self.owner = Some((self.next_generation, device.to_string()));
        Some(self.next_generation)
    }

    /// End the session of this generation, unless a newer one has started.
    /// Returns whether it was still the current session, so a session that
    /// was replaced doesn't tear down the link its successor is using.
    pub(crate) fn end(&mut self, generation: u64) -> bool {
        if self.owner.as_ref().is_some_and(|(g, _)| *g == generation) {
            self.owner = None;
            return true;
        }
        false
    }

    /// The device the current session belongs to.
    pub(crate) fn device(&self) -> Option<&str> {
        self.owner.as_ref().map(|(_, d)| d.as_str())
    }
}

/// Outbound Weave packets queued for the peer and not yet out of our hands.
///
/// Counted where a packet is queued and where it leaves (confirmed, given up
/// on, or dropped with its subscriber), so a waiter sees the real number at
/// once rather than a copy refreshed on a tick. The session waits on this
/// before it switches to the upgraded socket or lets the link go.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
#[derive(Clone)]
pub(crate) struct Outstanding(std::sync::Arc<tokio::sync::watch::Sender<usize>>);

#[cfg(any(target_os = "windows", target_os = "linux", test))]
impl Outstanding {
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(tokio::sync::watch::channel(0).0))
    }

    pub(crate) fn add(&self, n: usize) {
        self.0.send_modify(|count| *count += n);
    }

    pub(crate) fn done(&self, n: usize) {
        self.0.send_modify(|count| *count = count.saturating_sub(n));
    }

    pub(crate) fn count(&self) -> usize {
        *self.0.borrow()
    }

    /// Resolves once nothing is outstanding.
    pub(crate) async fn drained(&self) {
        let mut count = self.0.subscribe();
        // The sender lives as long as `self`, so this only ends at zero.
        if count.wait_for(|n| *n == 0).await.is_err() {
            debug!("BLE receiver: the outbound counter closed");
        }
    }
}

/// Split one outbound message into Weave data packets of at most
/// `packet_size` bytes, header included, numbering them from `counter`.
///
/// `packet_size` is what the ConnectionConfirm agreed, capped by what the link
/// carries in one notification. A packet bigger than the link's MTU fails on
/// the phone with GATT status 133, which is why it is not a fixed 509.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn weave_packets(
    bytes: &[u8],
    packet_size: usize,
    counter: &std::sync::atomic::AtomicU8,
) -> Vec<Vec<u8>> {
    // One byte of every packet is the header; never chunk by zero.
    let payload = packet_size.saturating_sub(1).max(1);
    let n = bytes.len().div_ceil(payload);
    bytes
        .chunks(payload)
        .enumerate()
        .map(|(i, chunk)| {
            let ctr = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0x07;
            let first = if i == 0 { 0x08 } else { 0 };
            let last = if i + 1 == n { 0x04 } else { 0 };
            let mut pkt = Vec::with_capacity(1 + chunk.len());
            pkt.push((ctr << 4) | first | last);
            pkt.extend_from_slice(chunk);
            pkt
        })
        .collect()
}

/// The Weave packet size to send with: the size both sides agreed, and no more
/// than one notification on this link holds (`link_max`, when known).
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn send_packet_size(agreed: u16, link_max: Option<u16>) -> usize {
    usize::from(link_max.map_or(agreed, |l| agreed.min(l)))
}

fn sha256_prefix(data: &[u8], n: usize) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()[..n].to_vec()
}

/// SHA-256(ascii-decimal of a random u32)[:2].
fn device_token() -> [u8; 2] {
    let n: u32 = rand::random();
    let v = sha256_prefix(n.to_string().as_bytes(), 2);
    [v[0], v[1]]
}

/// Service-ID bloom filter for "NearbySharing": google/nearby's `BloomFilter`
/// over 80 bits, five positions from the low 64 bits of
/// MurmurHash3_x64_128("NearbySharing", seed 0), position `p` stored in byte
/// `p / 8`, bit `p % 8`. The service ID never changes, so this is a constant;
/// the derivation was checked against an independent MurmurHash3.
const NEARBY_SHARING_BLOOM_FILTER: [u8; 10] =
    [0x20, 0x00, 0x00, 0x10, 0x02, 0x00, 0x00, 0x03, 0x00, 0x00];

/// The `BleAdvertisementHeader` for the 0xFEF3 service data:
/// `[version/extended/num_slots][bloom filter(10)][advertisement hash(4)][psm(2)]`.
///
/// Version 2, no extended advertising and one slot, so the phone reads the
/// full advertisement from slot 0 over GATT. The hash is SHA-256 of exactly the
/// bytes that characteristic serves; the phone checks the two match. No L2CAP,
/// so the PSM is 0.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub fn build_advertisement_header(full_advertisement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push((2u8 << 5) | 1);
    out.extend_from_slice(&NEARBY_SHARING_BLOOM_FILTER);
    out.extend_from_slice(&sha256_prefix(full_advertisement, 4));
    out.extend_from_slice(&[0, 0]);
    out
}

// ---- FULL (non-fast) advertisement, served over GATT -----------------------
//
// A peer that finds us connects and reads the slot-0 characteristic to get this.
// Unlike the fast form it is not size-constrained, so it carries the pieces the
// phone needs to build a listable ShareTarget - notably the **device name**.

/// SHA-256("NearbySharing")[:3] == FC 9F 5E.
fn service_id_hash() -> Vec<u8> {
    sha256_prefix(b"NearbySharing", 3)
}

/// Full endpoint info: bitfield, 16 bytes salt + metadata key, then the
/// length-prefixed UTF-8 device name. Visibility bit clear = visible/Everyone,
/// matching what `gen_mdns_endpoint_info` emits over mDNS.
///
/// The advertisement puts this whole block behind one length byte, so the name
/// only gets what is left after the 18 bytes in front of it, cut on a char
/// boundary so it stays valid UTF-8.
fn build_endpoint_info_full(device_type: u8, name: &str) -> Vec<u8> {
    const MAX_NAME_LEN: usize = u8::MAX as usize - 18;
    let mut len = name.len().min(MAX_NAME_LEN);
    while !name.is_char_boundary(len) {
        len -= 1;
    }

    let mut info = Vec::new();
    info.push(device_type << 1);
    let salt_and_meta: [u8; 16] = rand::random();
    info.extend_from_slice(&salt_and_meta);
    info.push(len as u8);
    info.extend_from_slice(&name.as_bytes()[..len]);
    info
}

/// Layer 3 non-fast: adds service_id_hash, bluetooth_mac, uwb size and the
/// extra field around the endpoint info.
fn build_connections_advertisement_full(endpoint_id: &[u8; 4], endpoint_info: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x23); // (version 1 << 5) | pcp 3
    out.extend_from_slice(&service_id_hash()); // FC 9F 5E
    out.extend_from_slice(endpoint_id);
    out.push(endpoint_info.len() as u8);
    out.extend_from_slice(endpoint_info);
    out.extend_from_slice(&[0u8; 6]); // bluetooth mac (reserved zeros)
    out.push(0x00); // uwb address size = 0
    out.push(0x00); // extra field (webrtc connectable = 0)
    out
}

/// Layer 2 non-fast: `[0x48][service_id_hash(3)][data_size u32 BE][data][token(2)]`.
fn build_medium_advertisement_full(data: &[u8], device_token: &[u8; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    // version kV2(2) | socket kV2(2) | fast=0 | second_profile=0
    out.push(((2u8 << 5) & 0xE0) | ((2u8 << 2) & 0x1C)); // 0x48
    out.extend_from_slice(&service_id_hash());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(device_token);
    out
}

/// The full BleAdvertisement to serve from the slot-0 GATT characteristic.
pub fn build_full_receiver_advertisement(
    endpoint_id: &[u8; 4],
    device_type: u8,
    name: &str,
) -> Vec<u8> {
    let endpoint_info = build_endpoint_info_full(device_type, name);
    let data = build_connections_advertisement_full(endpoint_id, &endpoint_info);
    build_medium_advertisement_full(&data, &device_token())
}

// ---- Parsing a peer's advertisement (the send path) ------------------------

/// Device name and type out of a **full** BleAdvertisement, as served from a
/// peer's slot-0 GATT characteristic.
///
/// The inverse of `build_full_receiver_advertisement`. Only the full form
/// carries a name - the fast form has no room for one, which is why a peer has
/// to be connected to and read rather than merely scanned.
///
/// Returns `None` if this isn't a NearbySharing advertisement, which is also how
/// the send path tells a Quick Share receiver from the many other devices
/// advertising the same 0xFEF3 copresence service.
/// What a peer's BleAdvertisement tells us about it.
#[derive(Debug, Clone)]
pub struct PeerAdvertisement {
    /// Absent in the fast form, which has no room for a name.
    pub name: Option<String>,
    pub device_type: u8,
    pub endpoint_id: [u8; 4],
}

/// Parse a peer's advertisement in either form.
///
/// Both are NearbySharing; which one you get depends on the platform. Android
/// fits the **fast** form (26 bytes) inside the 31-byte legacy advertisement and
/// broadcasts it inline. A connectable advertisement from Windows has no room
/// for it, so we advertise a header and serve the **full** form from a GATT
/// characteristic instead. A peer may therefore be identified straight from its
/// advertisement, with no connection needed.
///
/// `None` means this is not a NearbySharing advertisement at all, which is how
/// the send path tells a Quick Share peer from the many other devices sharing
/// the 0xFEF3 copresence service.
pub fn parse_peer_advertisement(adv: &[u8]) -> Option<PeerAdvertisement> {
    let first = *adv.first()?;
    // Layer 2 version byte: bit1 set = fast.
    if first & 0x02 != 0 {
        parse_fast_advertisement(adv)
    } else {
        parse_full_advertisement(adv).map(|(name, device_type)| PeerAdvertisement {
            name: Some(name),
            device_type,
            endpoint_id: [0; 4],
        })
    }
}

/// Fast layer 2: `[0x4A][layer3][device_token(2)]`, where layer 3 is
/// `[0x23][endpoint_id(4)][info_len][endpoint_info]`. No service_id_hash and no
/// name, so identification rests on the version/pcp bytes.
fn parse_fast_advertisement(adv: &[u8]) -> Option<PeerAdvertisement> {
    if adv.len() < 9 || adv[1] != 0x23 {
        return None;
    }
    let endpoint_id: [u8; 4] = adv.get(2..6)?.try_into().ok()?;
    let info_len = adv[6] as usize;
    let info = adv.get(7..7 + info_len)?;
    let device_type = (info.first()? >> 1) & 0x07;

    Some(PeerAdvertisement {
        name: None,
        device_type,
        endpoint_id,
    })
}

pub fn parse_full_advertisement(adv: &[u8]) -> Option<(String, u8)> {
    let hash = service_id_hash();

    // Layer 2: [version byte][service_id_hash(3)][data_size u32 BE][data][token(2)]
    if adv.len() < 8 || adv[1..4] != hash[..] {
        return None;
    }
    let data_size = u32::from_be_bytes([adv[4], adv[5], adv[6], adv[7]]) as usize;
    let data = adv.get(8..8 + data_size)?;

    // Layer 3: [version/pcp][service_id_hash(3)][endpoint_id(4)][info_len][endpoint_info]...
    if data.len() < 9 || data[1..4] != hash[..] {
        return None;
    }
    let info_len = data[8] as usize;
    let info = data.get(9..9 + info_len)?;

    // endpoint_info: [bitfield][16 salt+metadata key][name_len][name]
    if info.len() < 18 {
        return None;
    }
    let device_type = (info[0] >> 1) & 0x07;
    let name_len = info[17] as usize;
    let name = info.get(18..18 + name_len)?;

    Some((String::from_utf8_lossy(name).into_owned(), device_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fast advertisement as a phone broadcasts it: `[0x4A][0x23][id(4)]
    /// [info_len][info(17)][token(2)]`, with a Laptop's 0x16 bitfield.
    fn phone_fast_advertisement() -> Vec<u8> {
        let mut adv = vec![0x4A, 0x23];
        adv.extend_from_slice(b"NX8G");
        adv.push(17);
        adv.push(0x16);
        adv.extend_from_slice(&[0xAB; 16]);
        adv.extend_from_slice(&[0x74, 0xEB]);
        adv
    }

    #[test]
    fn header_matches_google_layout() {
        let full = build_full_receiver_advertisement(b"NX8G", 3, "AaronPC");
        let header = build_advertisement_header(&full);

        // Small enough to reach the phone from Windows, which the 26-byte fast
        // advertisement never did.
        assert_eq!(header.len(), 17);
        // Version 2, not extended, one slot.
        assert_eq!(header[0], 0x41);
        assert_eq!(header[1..11], NEARBY_SHARING_BLOOM_FILTER);
        assert_eq!(header[11..15], sha256_prefix(&full, 4)[..]);
        assert_eq!(header[15..], [0, 0]);
    }

    #[test]
    fn fast_advertisement_parses() {
        let peer = parse_peer_advertisement(&phone_fast_advertisement()).expect("parses");
        assert_eq!(peer.endpoint_id, *b"NX8G");
        assert_eq!(peer.device_type, 3);
        assert!(peer.name.is_none());
    }

    #[test]
    fn full_advertisement_round_trips() {
        let adv = build_full_receiver_advertisement(b"NX8G", 3, "Aaron's Pixel");
        let (name, device_type) = parse_full_advertisement(&adv).expect("should parse");
        assert_eq!(name, "Aaron's Pixel");
        assert_eq!(device_type, 3);
    }

    #[test]
    fn fast_advertisement_is_not_mistaken_for_full() {
        // The fast form has no name and no service_id_hash; parsing it as full
        // must fail rather than return garbage, since that is how the send path
        // filters non-Quick-Share peers.
        assert!(parse_full_advertisement(&phone_fast_advertisement()).is_none());
    }

    #[test]
    fn truncated_advertisement_does_not_panic() {
        let adv = build_full_receiver_advertisement(b"NX8G", 3, "Phone");
        for n in 0..adv.len() {
            let _ = parse_full_advertisement(&adv[..n]);
        }
    }

    #[test]
    fn a_long_name_still_fits_the_length_byte() {
        // 3-byte characters, so the cut has to land between them.
        let name = "名".repeat(200);
        let info = build_endpoint_info_full(3, &name);
        assert!(info.len() <= u8::MAX as usize);
        assert!(std::str::from_utf8(&info[18..]).is_ok());

        let adv = build_full_receiver_advertisement(&[1, 2, 3, 4], 3, &name);
        let (parsed, _) = parse_full_advertisement(&adv).expect("parses");
        assert!(name.starts_with(&parsed));
    }

    #[test]
    fn a_session_belongs_to_the_device_that_opened_it() {
        let mut active = ActivePeer::default();
        let first = active.start("phone-a", false).expect("free server");
        assert!(active.owns("phone-a"));
        assert!(!active.owns("phone-b"));

        // Another device can't take over while the owner is still there...
        assert_eq!(active.start("phone-b", true), None);
        assert!(active.owns("phone-a"));
        // ...but the owner can start over, as a phone retrying does.
        let retry = active.start("phone-a", true).expect("same device");
        assert!(retry > first);

        // The first session ending late must not free the retry's.
        assert!(!active.end(first));
        assert!(active.owns("phone-a"));
        assert!(active.end(retry));
        assert_eq!(active.device(), None);
    }

    #[test]
    fn packets_follow_the_negotiated_size() {
        let counter = std::sync::atomic::AtomicU8::new(6);
        let message = vec![0xab; 50];
        let packets = weave_packets(&message, 20, &counter);
        assert_eq!(packets.len(), 3);
        assert!(packets.iter().all(|p| p.len() <= 20));
        // First and last flags, and the counter wrapping mod 8.
        assert_eq!(packets[0][0], (6 << 4) | 0x08);
        assert_eq!(packets[1][0], 7 << 4);
        assert_eq!(packets[2][0], 0x04);
        let joined: Vec<u8> = packets.iter().flat_map(|p| p[1..].to_vec()).collect();
        assert_eq!(joined, message);

        // A single packet is both first and last.
        let one = weave_packets(&[1, 2, 3], 509, &counter);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0][0] & 0x0c, 0x0c);
    }

    #[test]
    fn the_link_caps_the_agreed_packet_size() {
        assert_eq!(send_packet_size(509, None), 509);
        assert_eq!(send_packet_size(509, Some(182)), 182);
        assert_eq!(send_packet_size(100, Some(244)), 100);
    }

    #[tokio::test]
    async fn outstanding_drains_on_the_last_completion() {
        let outstanding = Outstanding::new();
        outstanding.add(2);
        let waiter = tokio::spawn({
            let outstanding = outstanding.clone();
            async move { outstanding.drained().await }
        });
        outstanding.done(1);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        outstanding.done(1);
        waiter.await.expect("drained");
        // More completions than packets never wraps below zero.
        outstanding.done(1);
        assert_eq!(outstanding.count(), 0);
    }

    #[test]
    fn an_owner_that_left_does_not_block_the_next_device() {
        let mut active = ActivePeer::default();
        active.start("phone-a", false).expect("free server");
        // phone-a's link is gone but its session hasn't ended yet.
        assert!(active.start("phone-b", false).is_some());
        assert!(active.owns("phone-b"));
    }

    #[test]
    fn device_token_is_two_bytes() {
        assert_eq!(device_token().len(), 2);
    }
}
