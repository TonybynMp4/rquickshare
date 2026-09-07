#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[macro_use]
extern crate log;

use std::sync::{Arc, Mutex};

use rqs_lib::channel::{ChannelDirection, ChannelMessage};
use rqs_lib::{EndpointInfo, SendInfo, State, Visibility, RQS};
use store::get_startminimized;
#[cfg(target_os = "macos")]
use tauri::image::Image;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, Window, WindowEvent,
};
use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::{broadcast, mpsc, watch, Mutex as AsyncMutex};

use crate::logger::set_up_logging;
use crate::notification::{send_request_notification, send_temporarily_notification};
use crate::store::{
    get_device_name, get_download_path, get_port, get_realclose, get_visibility, init_default,
    set_visibility,
};

mod cmds;
mod logger;
mod notification;
mod store;

pub struct AppState {
    pub message_sender: broadcast::Sender<ChannelMessage>,
    pub dch_sender: broadcast::Sender<EndpointInfo>,
    pub visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
    pub sender_file: mpsc::Sender<SendInfo>,
    pub ble_receiver: broadcast::Receiver<()>,
    // Async mutex: `stop()` is awaited while held, which a std::sync::Mutex
    // cannot safely do (its guard isn't meant to live across an await point).
    pub rqs: AsyncMutex<RQS>,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Define tauri async runtime to be tokio
    tauri::async_runtime::set(tokio::runtime::Handle::current());

    // Build and run Tauri app
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            trace!("tauri_plugin_single_instance: instance already running");
            open_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        // `shell`'s `open` validates against a default allowlist of mailto:,
        // tel: and http(s):// - so a filesystem path is rejected as "not a
        // valid URI". `opener` is v2's replacement and handles paths properly.
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            cmds::change_download_path,
            cmds::change_logging_level,
            cmds::change_visibility,
            cmds::start_discovery,
            cmds::stop_discovery,
            cmds::get_hostname,
            cmds::get_advertised_name,
            cmds::get_device_name_override,
            cmds::change_device_name,
            cmds::send_payload,
            cmds::send_to_rs,
        ])
        .setup(|app| {
            // Setting up logging inside file for the app
            set_up_logging(app.app_handle())?;

            debug!("Starting setup of RQuickShare app");

            // Initialize default values for the store
            init_default(app.app_handle());

            // Initialize system Tray
            let name = MenuItemBuilder::new("RQuickShare")
                .enabled(false)
                .build(app)?;
            let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&name)
                .separator()
                .items(&[&show, &quit])
                .build()?;

            #[cfg(target_os = "macos")]
            let icon = Image::from_bytes(include_bytes!("../icons/tray.png")).unwrap();
            #[cfg(not(target_os = "macos"))]
            let icon = app.default_window_icon().unwrap().clone();

            let tray = TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "show" => {
                        trace!("tray_show");
                        open_main_window(app);
                    }
                    "quit" => {
                        trace!("tray_quit");
                        kill_app(app.app_handle());
                    }
                    _ => (),
                })
                .build(app)?;

            let _ = tray.set_icon_as_template(true);

            // Fetch initial configuration values
            let visibility = get_visibility(app.app_handle());
            let port_number = get_port(app.app_handle());
            let download_path = get_download_path(app.app_handle());
            let device_name = get_device_name(app.app_handle());

            let app_handle = app.app_handle().clone();
            // This is not optimal, but until I find a better way to init log
            // (inside file and stdout) before starting the lib, I'll keep it as
            // is. This allow me to get the whole log :)
            tokio::task::block_in_place(|| {
                tauri::async_runtime::block_on(async move {
                    trace!("Beginning of RQS start");
                    // Start the RQuickShare service
                    let mut rqs = RQS::new(visibility, port_number, download_path);
                    // Before `run`, so the very first mDNS announcement already
                    // carries the user's name instead of briefly advertising the
                    // hostname until the front end mounts and corrects it.
                    rqs.set_device_name(device_name);
                    let (sender_file, ble_receiver) = rqs.run().await.unwrap();

                    // Define state for tauri app
                    app_handle.manage(AppState {
                        message_sender: rqs.message_sender.clone(),
                        dch_sender: broadcast::channel(10).0,
                        visibility_sender: rqs.visibility_sender.clone(),
                        sender_file,
                        ble_receiver,
                        rqs: AsyncMutex::new(rqs),
                    });
                });
            });

            spawn_receiver_tasks(app.app_handle());
            Ok(())
        })
        .on_window_event(handle_window_event)
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            tauri::RunEvent::Ready => {
                trace!("RunEvent::Ready");
                if get_startminimized(app_handle) {
                    // Starting hidden is a convenience, so don't let it take the
                    // app down with it: on Linux these calls go through the
                    // window manager and can fail, and panicking here killed the
                    // app during startup rather than just leaving the window up.
                    #[cfg(not(target_os = "macos"))]
                    match app_handle.get_webview_window("main") {
                        Some(w) => {
                            if let Err(e) = w.hide() {
                                warn!("RunEvent::Ready: failed to start minimized: {e}");
                            }
                        }
                        None => warn!("RunEvent::Ready: no main window to minimize"),
                    }
                    #[cfg(target_os = "macos")]
                    if let Err(e) = app_handle.hide() {
                        warn!("RunEvent::Ready: failed to start minimized: {e}");
                    }
                }
            }
            tauri::RunEvent::ExitRequested { code, .. } => {
                trace!("RunEvent::ExitRequested");
                if code != Some(-1) {
                    kill_app(app_handle);
                }
            }
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => {
                trace!("RunEvent::Reopen");
                open_main_window(app_handle);
            }
            _ => {}
        });

    info!("Application stopped");
    Ok(())
}

fn spawn_receiver_tasks(app_handle: &AppHandle) {
    let capp_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let state: tauri::State<'_, AppState> = capp_handle.state();
        let mut receiver = state.message_sender.subscribe();

        loop {
            let rinfo = receiver.recv().await;

            match rinfo {
                Ok(info) => {
                    if info.state.as_ref().unwrap_or(&State::Initial)
                        == &State::WaitingForUserConsent
                    {
                        let name = info
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.source.as_ref())
                            .map(|source| source.name.clone())
                            .unwrap_or_else(|| "Unknown".to_string());
                        send_request_notification(name, info.id.clone(), &capp_handle);
                    }
                    rs2js_channelmessage(info, &capp_handle);
                }
                Err(e) => {
                    error!("RecvError: message_sender: {e}");
                }
            }
        }
    });

    let capp_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let state: tauri::State<'_, AppState> = capp_handle.state();
        let mut dch_receiver = state.dch_sender.subscribe();

        loop {
            let rinfo = dch_receiver.recv().await;

            match rinfo {
                Ok(info) => rs2js_endpointinfo(info, &capp_handle),
                Err(e) => {
                    error!("RecvError: dch_sender: {e}");
                }
            }
        }
    });

    let capp_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let state: tauri::State<'_, AppState> = capp_handle.state();
        let mut visibility_receiver = state.visibility_sender.lock().unwrap().subscribe();

        loop {
            let rinfo = visibility_receiver.changed().await;

            match rinfo {
                Ok(_) => {
                    let v = visibility_receiver.borrow_and_update();
                    let _ = set_visibility(&capp_handle, *v);
                }
                Err(e) => {
                    error!("RecvError: visibility_receiver: {e}");
                }
            }
        }
    });

    let capp_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let state: tauri::State<'_, AppState> = capp_handle.state();
        let mut ble_receiver = state.ble_receiver.resubscribe();
        let mut last_sent = std::time::Instant::now() - std::time::Duration::from_secs(120);

        loop {
            let rinfo = ble_receiver.recv().await;

            match rinfo {
                Ok(_) => {
                    let v = get_visibility(&capp_handle);
                    trace!("Tauri: ble received: {:?}", v);

                    if v == Visibility::Invisible
                        && last_sent.elapsed() >= std::time::Duration::from_secs(120)
                    {
                        send_temporarily_notification(&capp_handle);
                        last_sent = std::time::Instant::now();
                    }
                }
                Err(e) => {
                    error!("RecvError: ble_receiver: {e}");
                }
            }
        }
    });
}

fn handle_window_event(w: &Window, event: &WindowEvent) {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        if get_realclose(w.app_handle()) {
            trace!("handle_window_event: real close");
            return;
        }

        trace!("handle_window_event: prevent close");
        // Keep the close prevented even if hiding fails. `unwrap` here meant a
        // window-manager hiccup panicked the event handler, and since the panic
        // replaced `prevent_close()` the window then closed anyway - the
        // opposite of what the setting asks for.
        if let Err(e) = w.hide() {
            warn!("handle_window_event: failed to hide the window: {e}");
        }
        api.prevent_close();
    }
}

fn rs2js_channelmessage(message: ChannelMessage, manager: &AppHandle) {
    if message.direction == ChannelDirection::FrontToLib {
        return;
    }

    // Progress ticks arrive once per payload chunk - roughly 35/s over BLE -
    // and each one Debug-formats the whole struct (file list, destination,
    // device info) before it can be filtered. That formatting cost lands on
    // the thread draining the transfer, so keep the per-chunk case at `trace`
    // and log only the state transitions at `info`.
    if message.state == Some(State::ReceivingFiles) || message.state == Some(State::SendingFiles) {
        trace!("rs2js_channelmessage: {message:?}");
    } else {
        info!("rs2js_channelmessage: {message:?}");
    }
    manager.emit("rs2js_channelmessage", &message).unwrap();
}

fn rs2js_endpointinfo(message: EndpointInfo, manager: &AppHandle) {
    info!("rs2js_endpointinfo: {message:?}");
    manager.emit("rs2js_endpointinfo", &message).unwrap();
}

fn open_main_window(app_handle: &AppHandle) {
    if let Some(webview_window) = app_handle.get_webview_window("main") {
        // `unminimize` first, and it is not optional on Linux.
        //
        // Without it, restoring a *minimized* window here did nothing at all -
        // tray "Show", and relaunching into the single-instance hook, both
        // looked dead. Both calls it used to make are no-ops in that state, per
        // tao's GTK backend:
        //
        // - `show()` becomes `gtk_widget_show_all`, which does not deiconify;
        //   an iconified window stays iconified.
        // - `set_focus()` is wrapped in `if !minimized`, so it returns without
        //   sending the focus request.
        //
        // `unminimize()` is the one that reaches `gtk_window_deiconify`, and it
        // also clears the flag that was gating `set_focus`.
        //
        // Raising afterwards is still the compositor's call: under Wayland,
        // GNOME may answer a focus request from an unfocused client by marking
        // the window as needing attention rather than raising it. Nothing we
        // can override, but the window is at least back on screen.
        let _ = webview_window.unminimize();
        let _ = webview_window.show();
        let _ = webview_window.set_focus();
        return;
    }

    warn!("open_main_window: no main window found");
}

/// How long to let the service shut down cleanly before quitting regardless.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn kill_app(app_handle: &AppHandle) {
    let state: tauri::State<'_, AppState> = app_handle.state();

    // This runs on the thread driving the GTK main loop, so whatever happens
    // here the window is frozen until it returns. `stop()` waits on every
    // spawned task, and some of them block on channel receives with no timeout
    // of their own (the mDNS unregister acknowledgement, for one), so a stuck
    // task would leave the window painted on screen and unresponsive with no
    // way out but SIGKILL. Cap the wait: a clean unregister is nice, a
    // guaranteed quit is required.
    tokio::task::block_in_place(|| {
        tauri::async_runtime::block_on(async move {
            match tokio::time::timeout(SHUTDOWN_GRACE, async {
                state.rqs.lock().await.stop().await;
            })
            .await
            {
                Ok(()) => trace!("kill_app: service stopped cleanly"),
                Err(_) => warn!(
                    "kill_app: service did not stop within {}s, quitting anyway",
                    SHUTDOWN_GRACE.as_secs()
                ),
            }
        });
    });

    app_handle.exit(-1);
}
