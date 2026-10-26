//! Join a peer's access point, for the WIFI_HOTSPOT bandwidth upgrade when we
//! are the *sender*.
//!
//! The mirror of `hotspot_win.rs`. Receiving, we host an AP and the phone joins
//! us. Sending, the phone hosts - it brings one up even with WiFi off, which is
//! the normal Quick Share flow - and hands us credentials in
//! UPGRADE_PATH_AVAILABLE:
//!
//! ```text
//! medium: WifiHotspot, ssid: "DIRECT-C8-23666B", password: "27637516",
//! gateway: "192.168.49.1", port: 42321
//! ```
//!
//! `192.168.49.1` is the standard Android P2P/hotspot gateway. So this side of
//! the upgrade is a WiFi *client* problem, not a tethering one.
//!
//! Like the tethering manager, `WiFiAdapter` is apartment-threaded, so it stays
//! on a thread of its own and only the result crosses.

use std::sync::mpsc;

use tokio_util::sync::CancellationToken;
use windows::core::{GUID, HSTRING};
use windows::Devices::WiFi::{
    WiFiAdapter, WiFiAvailableNetwork, WiFiConnectionStatus, WiFiReconnectionKind,
};
use windows::Security::Credentials::PasswordCredential;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// The adapter a join connected and the network it joined, so cleanup can
/// disconnect exactly that and nothing else.
#[derive(Debug, Clone)]
pub struct JoinedAp {
    adapter_id: GUID,
    ssid: String,
}

/// Connect to `ssid` with `password`, blocking until it succeeds, fails or
/// `cancel` fires.
///
/// Rejoining the previous network afterwards is left to Windows: the connection
/// is made with `WiFiReconnectionKind::Manual`, so it is not persisted and the
/// adapter returns to its usual network when this one goes away.
///
/// The work runs on a thread of its own, which a dropped future cannot stop:
/// aborting the task that awaited it left the thread scanning for up to 30 s,
/// and it could still join the phone's network after the transfer was over.
/// So the thread watches `cancel` itself - it is checked after every scan and
/// before connecting, and a join that lands after it fired is undone.
pub async fn join(
    ssid: &str,
    password: &str,
    cancel: CancellationToken,
) -> Result<JoinedAp, anyhow::Error> {
    let (tx, rx) = mpsc::channel::<Result<JoinedAp, String>>();
    let (ssid, password) = (ssid.to_string(), password.to_string());

    std::thread::spawn(move || {
        // WinRT activation needs COM. Already initialized differently is not
        // fatal here; anything worse shows up as the first WinRT call failing.
        if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
            debug!("join: COM initialization: {e}");
        }

        let cancelled = || anyhow::anyhow!("the join was cancelled");
        let result = (|| -> Result<JoinedAp, anyhow::Error> {
            let adapters = WiFiAdapter::FindAllAdaptersAsync()?.join()?;
            if adapters.Size()? == 0 {
                return Err(anyhow::anyhow!("no WiFi adapter"));
            }
            let adapter = adapters.GetAt(0)?;
            let wanted = HSTRING::from(ssid.as_str());

            let find =
                |adapter: &WiFiAdapter| -> Result<Option<WiFiAvailableNetwork>, anyhow::Error> {
                    let networks = adapter.NetworkReport()?.AvailableNetworks()?;
                    for i in 0..networks.Size()? {
                        let n = networks.GetAt(i)?;
                        if n.Ssid()? == wanted {
                            return Ok(Some(n));
                        }
                    }
                    Ok(None)
                };

            // Find the peer's network: scan, and scan again as soon as a scan
            // finishes without it.
            //
            // A just-created WiFi-Direct group owner is often missing from the
            // first scan - measured: "network DIRECT-52-... was not found", a
            // single scan giving up too early and dropping the transfer back to
            // BLE. `ScanAsync().join()` returns when the results are in, so each
            // scan's completion starts the next; there is no wait between them
            // (this used to sleep up to 2 s, woken early by
            // AvailableNetworksChanged).
            //
            // The 30 s bound stands in for the phone never bringing its network
            // up, which nothing reports. Without one, scans would run back to
            // back for the whole of a BLE-speed transfer, which costs the
            // adapter's own connection. Ending here only means the transfer
            // stays on BLE.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let found = loop {
                if cancel.is_cancelled() || std::time::Instant::now() >= deadline {
                    break None;
                }
                adapter.ScanAsync()?.join()?;
                if let Some(n) = find(&adapter)? {
                    break Some(n);
                }
            };

            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let network = found
                .ok_or_else(|| anyhow::anyhow!("peer's network {ssid} never appeared in a scan"))?;

            let credential = PasswordCredential::new()?;
            credential.SetPassword(&HSTRING::from(password.as_str()))?;

            // Manual: do not persist it or auto-rejoin later. This is a
            // transfer-scoped network, not one the user chose.
            let outcome = adapter
                .ConnectWithPasswordCredentialAsync(
                    &network,
                    WiFiReconnectionKind::Manual,
                    &credential,
                )?
                .join()?;

            match outcome.ConnectionStatus()? {
                WiFiConnectionStatus::Success => {}
                other => return Err(anyhow::anyhow!("could not join {ssid}: status {other:?}")),
            }
            // Cancelled while connecting: nobody is left to clean this up, so
            // leave the phone's network again here.
            if cancel.is_cancelled() {
                adapter.Disconnect()?;
                return Err(cancelled());
            }
            Ok(JoinedAp {
                adapter_id: adapter.NetworkAdapter()?.NetworkAdapterId()?,
                ssid: ssid.clone(),
            })
        })();

        // The receiver is gone only if `join` was dropped, and then nobody
        // wants the answer.
        tx.send(result.map_err(|e| e.to_string())).ok();
    });

    // Blocking recv on a blocking pool thread, so the executor keeps running -
    // stalling it here would break the in-flight frames on the old channel.
    tokio::task::spawn_blocking(move || rx.recv())
        .await??
        .map_err(|e| anyhow::anyhow!(e))
}

/// Leave the phone's network after a transfer.
///
/// Without it the PC stays associated to the phone's transfer-scoped AP, which
/// also installs a default route that blackholes the PC's traffic through a
/// phone that has no internet.
///
/// Only the adapter the join used, and only while it is still on the phone's
/// network. This used to disconnect every WiFi adapter on the machine, so a
/// transfer that never joined anything - a small file done over BLE while the
/// join was still scanning - took the user off their own network.
pub async fn wifi_disconnect(joined: JoinedAp) {
    let outcome = tokio::task::spawn_blocking(move || -> Result<bool, anyhow::Error> {
        if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
            debug!("wifi_disconnect: COM initialization: {e}");
        }
        let adapters = WiFiAdapter::FindAllAdaptersAsync()?.join()?;
        for i in 0..adapters.Size()? {
            let adapter = adapters.GetAt(i)?;
            let network_adapter = adapter.NetworkAdapter()?;
            if network_adapter.NetworkAdapterId()? != joined.adapter_id {
                continue;
            }
            // Not connected at all (no profile) also means nothing to undo.
            let Ok(profile) = network_adapter
                .GetConnectedProfileAsync()
                .and_then(|op| op.join())
            else {
                return Ok(false);
            };
            let ssid = profile.WlanConnectionProfileDetails()?.GetConnectedSsid()?;
            if ssid != joined.ssid {
                // Windows has already moved it back to the user's network.
                return Ok(false);
            }
            adapter.Disconnect()?;
            return Ok(true);
        }
        Ok(false)
    })
    .await;

    match outcome {
        Ok(Ok(true)) => info!("Bandwidth upgrade: left the peer's network"),
        Ok(Ok(false)) => debug!("Bandwidth upgrade: already off the peer's network"),
        Ok(Err(e)) => warn!("Bandwidth upgrade: could not leave the peer's network: {e}"),
        Err(e) => warn!("Bandwidth upgrade: could not leave the peer's network: {e}"),
    }
}

/// Whether an interface holds an IPv4 address on the same /24 as `gateway`.
fn on_subnet_of(gateway: std::net::Ipv4Addr) -> bool {
    let want = gateway.octets();
    if_addrs::get_if_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|i| match i.ip() {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                o[0] == want[0] && o[1] == want[1] && o[2] == want[2] && !v4.is_loopback()
            }
            _ => false,
        })
}

/// A registration for IPv4 unicast address changes, undone on drop. Owns the
/// `Notify` the callback signals, which therefore outlives the registration.
struct AddressChanges {
    handle: windows::Win32::Foundation::HANDLE,
    changed: std::sync::Arc<tokio::sync::Notify>,
}

impl AddressChanges {
    fn register() -> Option<Self> {
        use windows::Win32::NetworkManagement::IpHelper::NotifyUnicastIpAddressChange;
        use windows::Win32::Networking::WinSock::AF_INET;

        let changed = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut handle = windows::Win32::Foundation::HANDLE::default();
        // SAFETY: the context points at the Notify `changed` holds, which
        // lives until `drop` has unregistered the callback.
        let result = unsafe {
            NotifyUnicastIpAddressChange(
                AF_INET,
                Some(on_address_change),
                Some(std::sync::Arc::as_ptr(&changed).cast()),
                false,
                &mut handle,
            )
        };
        match result.ok() {
            Ok(()) => Some(Self { handle, changed }),
            Err(e) => {
                warn!("Bandwidth upgrade: cannot watch for address changes: {e}");
                None
            }
        }
    }
}

// SAFETY: the handle is an opaque registration token, not memory; IP Helper
// lets any thread cancel it. The Notify is Send and Sync on its own.
unsafe impl Send for AddressChanges {}

impl Drop for AddressChanges {
    fn drop(&mut self) {
        use windows::Win32::NetworkManagement::IpHelper::CancelMibChangeNotify2;
        // SAFETY: the handle came from a successful registration and is
        // cancelled once; this waits out a callback already running, so the
        // Notify is not freed under it.
        if let Err(e) = unsafe { CancelMibChangeNotify2(self.handle) }.ok() {
            warn!("Bandwidth upgrade: could not stop watching address changes: {e}");
        }
    }
}

unsafe extern "system" fn on_address_change(
    context: *const core::ffi::c_void,
    _row: *const windows::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_ROW,
    _kind: windows::Win32::NetworkManagement::IpHelper::MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: the context is the Notify an `AddressChanges` keeps alive for
    // as long as this callback is registered.
    let changed = unsafe { &*context.cast::<tokio::sync::Notify>() };
    changed.notify_one();
}

/// Wait until an interface holds an address on the same /24 as `gateway`.
///
/// Association and addressing are separate: `ConnectAsync` returns Success as
/// soon as the link is up, but DHCP from the peer's AP takes a further moment.
/// Connecting before then has no route to the gateway and simply hangs -
/// measured as "timed out connecting to 192.168.49.1" two seconds after a
/// successful join.
///
/// Waits on Windows' address-change notification; this used to poll every
/// 300 ms for up to 30 s. No bound: the upgrade task that waits here ends when
/// the transfer does. Returns false only when the change can't be watched.
pub async fn wait_for_route(gateway: std::net::Ipv4Addr) -> bool {
    // Registered before the first look, so an address that arrives in
    // between still wakes the wait (`notify_one` keeps the permit).
    let Some(changes) = AddressChanges::register() else {
        return on_subnet_of(gateway);
    };
    while !on_subnet_of(gateway) {
        changes.changed.notified().await;
    }
    true
}
