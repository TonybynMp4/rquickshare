//! Windows WiFi Direct group owner for the WIFI_DIRECT bandwidth-upgrade
//! medium, using **device-name discovery** (`WIFI_DIRECT_WITH_DEVICE_NAME`).
//!
//! **Why device name and not ssid/passphrase.** google/nearby's proto is
//! explicit, and it is the whole story:
//!
//! ```proto
//! enum WifiDirectAuthType {
//!   // WifiDirect type that uses ssid/password for authentication. Android
//!   // supports this type, but Windows does not.
//!   WIFI_DIRECT_WITH_PASSWORD = 1;
//!   // WifiDirect type that uses device_name for discovery and connect.
//!   // Android and Windows both support this type.
//!   WIFI_DIRECT_WITH_DEVICE_NAME = 3;
//! }
//! ```
//!
//! **Windows cannot host the ssid/password type.** An earlier version of this
//! file advertised a legacy soft-AP (`LegacySettings`) with an SSID and
//! passphrase; the Pixel accepted the offer, spent 12s failing to associate, and
//! reported `CONNECT_TO_NETWORK_FAILED [TIMEOUT]` / `GROUP_REMOVED` every single
//! time. It was never going to work: the phone does Wi-Fi P2P *device discovery*
//! by name (`WifiP2pManager`, `startGroupRole:CLIENT`), and we were broadcasting
//! an AP it wasn't looking for. A manual join from the phone's WiFi list *did*
//! work, which is what made the legacy AP look proven - that tested the legacy
//! path, not the one Nearby uses.
//!
//! Mirrors `internal/platform/implementation/windows/wifi_direct_medium.cc`:
//! autonomous GO, no legacy settings, advertise the uppercased computer name,
//! and accept the peer through `ConnectionRequested` -> custom pairing
//! (PushButton/ConfirmOnly) -> `WiFiDirectDevice::FromIdAsync`. The real IPs
//! come from `GetConnectionEndpointPairs()` on that device, not from ICS.

use std::sync::{Arc, Mutex};

use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationCustomPairing, DeviceInformationPairing,
    DevicePairingProtectionLevel, DevicePairingRequestedEventArgs, DevicePairingResultStatus,
    DeviceUnpairingResultStatus,
};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
    WiFiDirectConfigurationMethod, WiFiDirectConnectionListener, WiFiDirectConnectionParameters,
    WiFiDirectConnectionRequestedEventArgs, WiFiDirectDevice, WiFiDirectPairingProcedure,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// What the peer needs to find and join this group.
///
/// No SSID, no passphrase, no gateway: with device-name discovery the peer finds
/// us by name over Wi-Fi P2P and learns our address from the P2P connection
/// itself, so none of those are ours to supply.
#[derive(Debug, Clone)]
pub struct WifiDirectCreds {
    /// The Wi-Fi P2P device name the peer discovers us by. google uppercases the
    /// computer name; match that.
    pub device_name: String,
    /// Legacy AP SSID. Empty on a device-name-only group.
    pub ssid: String,
    /// Legacy AP passphrase. Empty on a device-name-only group.
    pub passphrase: String,
    /// The group owner's IP on the ICS subnet (192.168.137.1).
    pub gateway: String,
}

pub struct WindowsWifiDirect {
    publisher: WiFiDirectAdvertisementPublisher,
    /// Held so the listener outlives `start()`. Dropping it stops Windows
    /// answering P2P association attempts.
    listener: WiFiDirectConnectionListener,
    /// The `ConnectionRequested` registration, removed again in `stop()`.
    connection_requested: i64,
    /// Accepted peers. A `WiFiDirectDevice` *is* the connection - drop it and
    /// Windows tears the association down, so they have to live as long as the
    /// group does.
    peers: Arc<Mutex<Vec<WiFiDirectDevice>>>,
}

impl std::fmt::Debug for WindowsWifiDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsWifiDirect").finish_non_exhaustive()
    }
}

impl WindowsWifiDirect {
    /// Start an autonomous WiFi Direct group owner and return the device name
    /// to advertise in an UPGRADE_PATH_AVAILABLE frame.
    ///
    /// Blocking: the WinRT calls take a moment. Call from `spawn_blocking`,
    /// never on the async executor - stalling it breaks the in-flight handshake
    /// frames.
    pub fn start() -> Result<(Self, WifiDirectCreds), anyhow::Error> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let advertisement = publisher.Advertisement()?;

        // Without this there is no group: the publisher only advertises presence
        // and waits for a peer to negotiate group ownership, so no group and no
        // virtual adapter appear - while Start() still reports Started, because
        // the *advertisement* did start.
        advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;

        // Discoverability: google uses Normal ("highly discoverable so long as
        // the app is in the foreground"), but we are a background service and
        // the peer's timing is tight - logcat shows it firing its first
        // P2P-GROUP-FORMATION-FAILURE *before* the P2P-DEVICE-FOUND for us lands,
        // i.e. attempting formation blind and only discovering us afterwards.
        // Intensive advertises harder at the cost of power, which we can afford
        // for the ~15s a transfer lasts.
        //
        // RQS_WIFI_DIRECT_DISCOVERABILITY=normal to compare.
        let intensive = std::env::var("RQS_WIFI_DIRECT_DISCOVERABILITY")
            .map(|v| v != "normal")
            .unwrap_or(true);
        advertisement.SetListenStateDiscoverability(if intensive {
            WiFiDirectAdvertisementListenStateDiscoverability::Intensive
        } else {
            WiFiDirectAdvertisementListenStateDiscoverability::Normal
        })?;

        // Legacy AP *as well as* the P2P group - but this is now suspect.
        //
        // google's Windows GO never enables this. We did, because the Pixel's
        // shipped GMS rejects a device-name-only frame ("missing ssid or not in
        // correct format") while current google/nearby refuses ssid/password
        // outright - the phone is older than the tree.
        //
        // The suspicion: with legacy enabled, the phone's P2P discovery sees us
        // with `group_capab=0x88` - IP Address Allocation + Intra-BSS
        // Distribution, but **bit 0, P2P Group Owner, is not set**. We are
        // telling it we're not a GO while running an autonomous GO. A client
        // told that would try GO *negotiation* instead of a plain join, and an
        // autonomous GO won't negotiate - which is a sub-second
        // P2P-GROUP-FORMATION-FAILURE, exactly what we get.
        //
        // **PROVEN 2026-07-16: enabling LegacySettings suppresses the GO bit.**
        // Same phone, same code, one property toggled, measured in the peer's
        // own P2P discovery:
        //     legacy ON : group_capab=0x88   (IP alloc + intra-BSS)
        //     legacy OFF: group_capab=0x8b   (+ bit 0 = P2P GROUP OWNER)
        // With legacy on we ran an autonomous GO that told every peer "I am not
        // a group owner", so clients attempted GO *negotiation* instead of a
        // plain join - and an autonomous GO won't negotiate. That is the
        // sub-second P2P-GROUP-FORMATION-FAILURE we chased all day. The BSSID
        // confirms it: legacy off splits it from the device address
        // (72:08:10:a2:6a:b6 vs p2p_dev_addr 70:08:10:a2:6a:b7), which is what a
        // real GO looks like on the air.
        //
        // Default OFF, matching google's Windows client. `legacy.Ssid()` and
        // `Passphrase()` still read back the group's credentials with it
        // disabled, so we lose nothing - we can set the GO bit *and* populate
        // the ssid the phone's older GMS insists on.
        //
        // RQS_WIFI_DIRECT_LEGACY=1 restores the old behaviour for comparison.
        let legacy_enabled = std::env::var("RQS_WIFI_DIRECT_LEGACY")
            .map(|v| v == "1")
            .unwrap_or(false);
        let legacy = advertisement.LegacySettings()?;
        if legacy_enabled {
            warn!("WiFi Direct: legacy settings ENABLED - this suppresses the GO bit");
            legacy.SetIsEnabled(true)?;
        }

        // Accept incoming P2P clients. This is the *only* way a peer joins a
        // device-name group, so it must exist before Start() or a request can
        // arrive with nothing listening.
        let peers: Arc<Mutex<Vec<WiFiDirectDevice>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = WiFiDirectConnectionListener::new()?;
        let peers_for_handler = Arc::clone(&peers);
        let connection_requested =
            listener.ConnectionRequested(&TypedEventHandler::<
                WiFiDirectConnectionListener,
                WiFiDirectConnectionRequestedEventArgs,
            >::new(move |_sender, args| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let request = args.GetConnectionRequest()?;
                let device_info = request.DeviceInformation()?;
                let name = device_info
                    .Name()
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let id = device_info.Id()?;
                info!("*** WiFi Direct: ConnectionRequested from {name:?} ***");

                if let Err(e) = accept_peer(&device_info, &id, &name, &peers_for_handler) {
                    warn!("WiFi Direct: failed to accept {name:?}: {e}");
                }
                Ok(())
            }))?;

        publisher.Start()?;

        // Status: Created=0, Started=1, Aborted=2, Stopped=3.
        let mut status = -1;
        for _ in 0..40 {
            status = publisher.Status()?.0;
            if status == 1 || status == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if status != 1 {
            return Err(anyhow::anyhow!(
                "WiFi Direct publisher did not start (status {status})"
            ));
        }

        // The peer discovers us by this name over Wi-Fi P2P. google takes the OS
        // computer name and uppercases it; match that exactly, since the phone
        // compares names to decide which group owner is the one it was promised.
        let device_name = ::hostname::get()?.to_string_lossy().to_uppercase();

        // WinRT hands back NUL-padded HSTRINGs and `to_string()` keeps the
        // padding. A WPA2 passphrase must be 8-63 *printable* ASCII, so an
        // untrimmed one is rejected at validation before the radio is touched -
        // the phone echoed `password: "PuLLMSIN\0"` straight back at us.
        // Read them even when legacy is off - the group has an SSID either way,
        // and if WinRT hands it over we can still populate the field the phone
        // insists on. Tolerate failure rather than propagate: with legacy
        // disabled these may legitimately be unavailable.
        let ssid = legacy
            .Ssid()
            .map(|s| s.to_string().trim_end_matches('\0').to_string())
            .unwrap_or_default();
        let passphrase = legacy
            .Passphrase()
            .and_then(|p| p.Password())
            .map(|s| s.to_string().trim_end_matches('\0').to_string())
            .unwrap_or_default();

        // The ICS address isn't there the instant Start() returns.
        let gateway = wait_for_gateway_ip().unwrap_or_else(|| {
            warn!("WiFi Direct: no 192.168.137.x address appeared; assuming the ICS default");
            "192.168.137.1".to_string()
        });

        info!(
            "WiFi Direct: group owner up, device_name={device_name:?} ssid={ssid:?} gateway={gateway}"
        );

        Ok((
            Self {
                publisher,
                listener,
                connection_requested,
                peers,
            },
            WifiDirectCreds {
                device_name,
                ssid,
                passphrase,
                gateway,
            },
        ))
    }

    fn stop(&self) -> Result<(), anyhow::Error> {
        // No new peers once we're stopping.
        if let Err(e) = self
            .listener
            .RemoveConnectionRequested(self.connection_requested)
        {
            warn!("WiFi Direct: failed to remove the connection handler: {e}");
        }
        // Drop the accepted peers first: each WiFiDirectDevice is a live
        // association, and they should go before the advertisement that carries
        // them rather than be torn down by process exit.
        match self.peers.lock() {
            Ok(mut guard) => guard.clear(),
            Err(e) => warn!("WiFi Direct: peers mutex poisoned on stop: {e}"),
        }
        self.publisher.Stop()?;
        Ok(())
    }
}

impl Drop for WindowsWifiDirect {
    fn drop(&mut self) {
        // Unlike the hotspot (which leaked via mem::forget and stayed on across
        // app restarts), tie the group's lifetime to this handle.
        if let Err(e) = self.stop() {
            warn!("WiFi Direct: failed to stop publisher on drop: {e}");
        }
    }
}

/// Pair with an incoming P2P client and hold the resulting connection.
///
/// Mirrors `WifiDirectMedium::OnConnectionRequested`. The sequence is not
/// obvious and none of it is optional:
///
/// 1. If Windows already has a stale pairing for this peer, **unpair first**.
///    A leftover record makes the new pairing fail or hang. google sleeps 3s
///    afterwards "to allow WiFi driver to stabilize" and re-reads the device
///    info, because the old handle is invalid once unpaired.
/// 2. Pair with `PushButton` config (which maps to `ConfirmOnly`), group owner
///    intent 14 (high - we want to stay GO), procedure `Invitation`.
/// 3. Only *then* `WiFiDirectDevice::FromIdAsync`. There is no explicit accept
///    call; resolving the device is the acceptance, and the returned device *is*
///    the connection - drop it and the peer is disconnected.
/// 4. `GetConnectionEndpointPairs()` is where the real local/remote IPs come
///    from. Not ICS, not 192.168.137.1.
fn accept_peer(
    device_info: &DeviceInformation,
    id: &windows::core::HSTRING,
    name: &str,
    peers: &Arc<Mutex<Vec<WiFiDirectDevice>>>,
) -> Result<(), anyhow::Error> {
    let pairing = device_info.Pairing()?;

    let paired = if pairing.IsPaired().unwrap_or(false) {
        info!("WiFi Direct: {name:?} already paired; unpairing to clear stale state");
        let result = pairing.UnpairAsync()?.join()?;
        let status = result.Status()?;
        // NB: unpairing reports through `DeviceUnpairingResultStatus`, a
        // different enum from the `DevicePairingResultStatus` used for pairing.
        if status == DeviceUnpairingResultStatus::Unpaired
            || status == DeviceUnpairingResultStatus::AlreadyUnpaired
        {
            // Let the driver settle, then re-read: the pairing handle we hold is
            // stale now.
            std::thread::sleep(std::time::Duration::from_secs(3));
            let refreshed = DeviceInformation::CreateFromIdAsync(id)?.join()?;
            request_pair(&refreshed.Pairing()?)?
        } else {
            info!("WiFi Direct: unpair of {name:?} returned {status:?}; assuming still paired");
            true
        }
    } else {
        request_pair(&pairing)?
    };

    if !paired {
        return Err(anyhow::anyhow!("pairing with {name:?} failed"));
    }

    let device = WiFiDirectDevice::FromIdAsync(id)?.join()?;
    let endpoint_pairs = device.GetConnectionEndpointPairs()?;
    if endpoint_pairs.Size()? > 0 {
        let pair = endpoint_pairs.GetAt(0)?;
        let local = pair.LocalHostName()?.DisplayName()?.to_string();
        let remote = pair.RemoteHostName()?.DisplayName()?.to_string();
        info!("*** WiFi Direct: accepted {name:?} - local {local}, remote {remote} ***");
    } else {
        warn!("WiFi Direct: accepted {name:?} but no connection endpoint pairs");
    }

    match peers.lock() {
        Ok(mut guard) => guard.push(device),
        Err(e) => warn!("WiFi Direct: peers mutex poisoned: {e}"),
    }
    Ok(())
}

/// Drive the custom pairing handshake. Group owner intent 14 keeps us the GO.
fn request_pair(pairing: &DeviceInformationPairing) -> Result<bool, anyhow::Error> {
    let config_method = WiFiDirectConfigurationMethod::PushButton;

    let params = WiFiDirectConnectionParameters::new()?;
    params.SetGroupOwnerIntent(14)?;
    params
        .PreferenceOrderedConfigurationMethods()?
        .Append(config_method)?;
    params.SetPreferredPairingProcedure(WiFiDirectPairingProcedure::Invitation)?;

    let kinds = WiFiDirectConnectionParameters::GetDevicePairingKinds(config_method)?;

    let custom: DeviceInformationCustomPairing = pairing.Custom()?;
    custom.PairingRequested(&TypedEventHandler::<
        DeviceInformationCustomPairing,
        DevicePairingRequestedEventArgs,
    >::new(|_sender, args| {
        if let Some(args) = args.as_ref() {
            // PushButton maps to ConfirmOnly: there is no pin to show or type,
            // we just have to say yes. Without this the pairing silently stalls.
            info!(
                "WiFi Direct: pairing requested, kind {:?}; accepting",
                args.PairingKind()
            );
            args.Accept()?;
        }
        Ok(())
    }))?;

    let result = custom
        .PairWithProtectionLevelAndSettingsAsync(
            kinds,
            DevicePairingProtectionLevel::Default,
            &params,
        )?
        .join()?;
    let status = result.Status()?;
    if status == DevicePairingResultStatus::Paired
        || status == DevicePairingResultStatus::AlreadyPaired
    {
        Ok(true)
    } else {
        warn!("WiFi Direct: pairing result {status:?}");
        Ok(false)
    }
}

/// The group owner lives on the 192.168.137.0/24 ICS subnet - the same one the
/// Mobile Hotspot uses. The adapter is a plain `Wi-Fi N` entry with no "Direct"
/// in its name, so match on the subnet rather than the interface name.
///
/// Only meaningful for the legacy/ssid path. A device-name peer learns our
/// address from `GetConnectionEndpointPairs()` on the paired device instead.
fn wait_for_gateway_ip() -> Option<String> {
    for _ in 0..20 {
        if let Some(ip) = gateway_ip() {
            return Some(ip);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

fn gateway_ip() -> Option<String> {
    for iface in if_addrs::get_if_addrs().ok()? {
        if let std::net::IpAddr::V4(v4) = iface.ip() {
            let o = v4.octets();
            if o[0] == 192 && o[1] == 168 && o[2] == 137 {
                return Some(v4.to_string());
            }
        }
    }
    None
}
