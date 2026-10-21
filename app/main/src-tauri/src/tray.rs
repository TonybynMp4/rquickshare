use rqs_lib::Visibility;
#[cfg(target_os = "macos")]
use tauri::image::Image;
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    tray::TrayIconBuilder,
    App, AppHandle, Manager, Wry,
};

use crate::store::get_visibility;
use crate::{kill_app, open_main_window, AppState};

const VISIBILITIES: [(Visibility, &str, &str); 3] = [
    (Visibility::Visible, "visible", "Always visible"),
    (Visibility::Invisible, "invisible", "Hidden from everyone"),
    (
        Visibility::Temporarily,
        "temporarily",
        "Temporarily visible",
    ),
];

/// The tray's visibility items, kept so they can be re-ticked whenever the
/// visibility changes, whoever changed it.
struct TrayVisibility(Vec<(Visibility, CheckMenuItem<Wry>)>);

pub fn build_tray(app: &App) -> tauri::Result<()> {
    let name = MenuItemBuilder::new("RQuickShare")
        .enabled(false)
        .build(app)?;
    let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    // Store defaults are initialised before this runs, so this is the value
    // RQS is about to start with.
    let current = get_visibility(app.app_handle());
    let visibility_items = VISIBILITIES
        .iter()
        .map(|(v, id, text)| {
            CheckMenuItemBuilder::with_id(*id, *text)
                .checked(*v == current)
                .build(app)
                .map(|item| (*v, item))
        })
        .collect::<tauri::Result<Vec<_>>>()?;

    // A fixed label: on Linux, muda's `Submenu::set_text` doesn't reach the
    // label GTK shows (it calls `set_label` on an item whose child is a box,
    // which GTK ignores), so a label carrying the state would go stale.
    let mut submenu = SubmenuBuilder::new(app, "Visibility");
    for (_, item) in &visibility_items {
        submenu = submenu.item(item);
    }
    let submenu = submenu.build()?;

    let menu = MenuBuilder::new(app)
        .item(&name)
        .separator()
        .item(&show)
        .item(&submenu)
        .separator()
        .item(&quit)
        .build()?;
    app.manage(TrayVisibility(visibility_items));

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
            id => {
                if let Some((v, _, _)) = VISIBILITIES.iter().find(|(_, vid, _)| *vid == id) {
                    trace!("tray_visibility: {v:?}");
                    change_visibility(app, *v);
                }
            }
        })
        .build(app)?;

    let _ = tray.set_icon_as_template(true);

    Ok(())
}

/// Same effect as `cmds::change_visibility`, without awaiting the RQS lock from
/// a sync menu handler. The visibility watcher in `spawn_receiver_tasks` then
/// saves it, tells the window and re-ticks the tray.
fn change_visibility(app: &AppHandle, v: Visibility) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // `send_modify` notifies even when the value is unchanged. That matters:
    // clicking the ticked item makes the menu untick it, and only the
    // watcher's `show_visibility` puts the tick back.
    state
        .visibility_sender
        .lock()
        .unwrap()
        .send_modify(|state| *state = v);
}

/// Tick the item for `v` and untick the others.
pub fn show_visibility(app: &AppHandle, v: Visibility) {
    let Some(items) = app.try_state::<TrayVisibility>() else {
        return;
    };
    for (item_v, item) in &items.0 {
        if let Err(e) = item.set_checked(*item_v == v) {
            warn!("show_visibility: {e}");
        }
    }
}
