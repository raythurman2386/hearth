//! Settings → Devices: the device registry — name, platform, last-seen,
//! presence dot, a "This device" badge, click-to-copy id, Rename (`Mutate`
//! `renameDevice`), and Remove for other machines (`Mutate` `deleteDevice`).
//!
//! Ported from `crates/ui/src/settings/devices.rs`; the gpui entity/tasks are
//! replaced by a page struct plus one-shot bridge jobs whose results are
//! drained by [`DevicesPage::poll`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hearth_proto::{Device, WorkspaceScope};
use hearth_rpc::methods;
use rune::{El, KeyEvent, TextStyle, Tx, col, el, row, text};

use crate::icons;
use crate::pickers::Loadable;
use crate::state::App;
use crate::theme::Theme;

use super::widgets;

/// A device that pinged within this window shows a presence dot (engines
/// heartbeat every 15s; 70s tolerates a couple of missed beats).
pub const DEVICE_ONLINE_WINDOW_SECS: i64 = 70;

/// How long the id chip says "Copied".
pub const COPIED_CHIP_MS: u64 = 1500;

/// Presence: last-seen within the online window (future timestamps count). Pure.
pub fn device_online(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_seen
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= DEVICE_ONLINE_WINDOW_SECS)
}

/// Compact last-seen line. Pure.
pub fn format_last_seen(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = last_seen else {
        return "never seen".to_string();
    };
    let secs = now.signed_duration_since(at).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Turn a device-mutation RPC error into the line Settings shows.
///
/// An app newer than the daemon it talks to asks for an op that build does not
/// have; serde rejects it while deserializing the tagged `MutateParams` enum.
/// That looks like a bug, so name the real cause — and keep the raw engine
/// text appended for debugging.
pub fn device_mutation_error(action: &str, err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    let version_skew = ["unknown variant", "unknown field", "unknown operation"]
        .iter()
        .any(|needle| lower.contains(needle));
    if version_skew {
        format!(
            "{action} failed: the running engine is older than this app — restart or update the engine. ({err})"
        )
    } else {
        format!("{action} failed: {err}")
    }
}

/// Scope-aware copy: a local registry describes only the active local
/// workspace and must not imply that account device metadata is already live.
pub fn devices_subtitle(scope: Option<WorkspaceScope>) -> &'static str {
    match scope {
        Some(WorkspaceScope::Local) => {
            "Manage device details stored in this local workspace. Remove machines you no longer use."
        }
        Some(WorkspaceScope::Development) | None => {
            "Manage device names for this workspace. Remove machines you no longer use."
        }
    }
}

/// Human platform label (hearth settings.devices.tsx `platformLabel`).
pub fn platform_label(platform: &str) -> &str {
    match platform {
        "macos" | "darwin" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "web" => "Web",
        "ios" => "iOS",
        "android" => "Android",
        other => other,
    }
}

/// Short device id for the click-to-copy chip (`abcd1234…wxyz`).
pub fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

/// The platform tile glyph.
fn platform_icon(platform: &str) -> &'static rune::Icon {
    match platform {
        "macos" | "darwin" => &icons::LAPTOP,
        "web" => &icons::GLOBAL,
        "ios" | "android" => &icons::SMARTPHONE,
        _ => &icons::MONITOR,
    }
}

/// The Rename dialog's input.
pub struct RenameDialog {
    pub device_id: String,
    pub input: rune::TextEdit,
}

impl std::fmt::Debug for RenameDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenameDialog")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// Result slot for a rename/remove call.
type MutationSlot = Arc<Mutex<Option<(u64, Result<(), String>)>>>;

/// View state for the Devices page.
#[derive(Debug, Default)]
pub struct DevicesPage {
    pub rename: Option<RenameDialog>,
    /// Device id pending a remove confirmation.
    pub delete_confirm: Option<String>,
    /// Device id whose id-chip shows "Copied" right now.
    pub copied: Option<String>,
    pub busy: bool,
    pub error: Option<String>,
    /// The registry snapshot last handed to [`DevicesPage::sync`]. The view
    /// still reads the live `app.devices`; this slot gives the page the same
    /// `Loadable` shape as its siblings.
    pub load: Loadable<Vec<Device>>,
    mutation: MutationSlot,
    generation: u64,
    copied_at: Option<Instant>,
}

impl DevicesPage {
    /// Copy the live registry into the page's `Loadable` slot.
    pub fn sync(&mut self, devices: &[Device]) {
        self.load = Loadable::Ready(devices.to_vec());
    }

    /// Drain a finished rename/remove and expire the copied-id chip.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let done = self.mutation.lock().unwrap().take();
        if let Some((generation, result)) = done
            && generation == self.generation
        {
            self.busy = false;
            if let Err(message) = result {
                self.error = Some(message);
            }
            changed = true;
        }
        if let Some(at) = self.copied_at
            && at.elapsed() >= Duration::from_millis(COPIED_CHIP_MS)
        {
            self.copied = None;
            self.copied_at = None;
            changed = true;
        }
        changed
    }
}

fn next_generation(page: &mut DevicesPage) -> u64 {
    page.generation = page.generation.wrapping_add(1);
    page.generation
}

/// Spawn a `Mutate` call; the result lands in the page slot.
fn spawn_mutation(
    app: &App,
    slot: MutationSlot,
    generation: u64,
    params: serde_json::Value,
    action: &'static str,
) -> bool {
    let Some(client) = app.engine.as_ref().map(|engine| engine.client.clone()) else {
        return false;
    };
    let bridge = app.bridge.clone();
    app.bridge.spawn(async move {
        let result = client
            .call(methods::MUTATE, params)
            .await
            .map(|_| ())
            .map_err(|err| device_mutation_error(action, &err.to_string()));
        *slot.lock().unwrap() = Some((generation, result));
        bridge.wake();
    });
    true
}

/// Open the Rename dialog seeded with the device's current name.
pub fn open_rename(app: &mut App, device_id: String, current: String) {
    app.settings_ui.devices.rename = Some(RenameDialog {
        device_id,
        input: rune::TextEdit::new(current),
    });
}

/// Commit the Rename dialog.
pub fn submit_rename(app: &mut App) {
    let Some(dialog) = app.settings_ui.devices.rename.take() else {
        return;
    };
    let name = dialog.input.text.trim().to_string();
    if name.is_empty() {
        return;
    }
    let params = serde_json::json!({
        "op": "renameDevice",
        "deviceId": dialog.device_id,
        "name": name,
    });
    let page = &mut app.settings_ui.devices;
    page.error = None;
    page.busy = true;
    let generation = next_generation(page);
    let slot = page.mutation.clone();
    if !spawn_mutation(app, slot, generation, params, "Rename") {
        app.settings_ui.devices.busy = false;
        app.settings_ui.devices.error = Some("Not connected to the engine".to_string());
    }
}

/// Confirm the Remove dialog.
pub fn submit_delete(app: &mut App) {
    let Some(device_id) = app.settings_ui.devices.delete_confirm.take() else {
        return;
    };
    let params = serde_json::json!({
        "op": "deleteDevice",
        "deviceId": device_id,
    });
    let page = &mut app.settings_ui.devices;
    page.error = None;
    page.busy = true;
    let generation = next_generation(page);
    let slot = page.mutation.clone();
    if !spawn_mutation(app, slot, generation, params, "Remove") {
        app.settings_ui.devices.busy = false;
        app.settings_ui.devices.error = Some("Not connected to the engine".to_string());
    }
}

fn copy_id(app: &mut App, tx: &mut Tx, device_id: String) {
    tx.copy(device_id.clone());
    let page = &mut app.settings_ui.devices;
    page.copied = Some(device_id);
    page.copied_at = Some(Instant::now());
}

fn dismiss_error(app: &mut App) {
    app.settings_ui.devices.error = None;
}

fn request_delete(app: &mut App, device_id: String) {
    app.settings_ui.devices.delete_confirm = Some(device_id);
}

fn cancel_delete(app: &mut App) {
    app.settings_ui.devices.delete_confirm = None;
}

fn cancel_rename(app: &mut App) {
    app.settings_ui.devices.rename = None;
}

fn rename_dialog(page: &DevicesPage, theme: &Theme) -> Option<rune_kit::Dialog> {
    let dialog = page.rename.as_ref()?;
    let kit = crate::shell::shell_kit(theme);
    let input = rune_kit::TextInput::new(&dialog.input)
        .placeholder("Device name")
        .label("Device name")
        .text_style(TextStyle::new(13.0, 400.0))
        .on_edit(|app: &mut App| {
            &mut app
                .settings_ui
                .devices
                .rename
                .as_mut()
                .expect("rename dialog is open")
                .input
        })
        .on_submit(|app: &mut App, _tx: &mut Tx| submit_rename(app))
        .build(&kit)
        .autofocus();
    let body = col().w_full().mt(12.0).child(input).child(
        row()
            .mt(16.0)
            .w_full()
            .justify_end()
            .gap(8.0)
            .child(
                rune_kit::Button::new("Cancel")
                    .variant(rune_kit::ButtonVariant::Ghost)
                    .on_press(|app: &mut App| cancel_rename(app))
                    .build(&kit),
            )
            .child(
                rune_kit::Button::new("Rename")
                    .variant(rune_kit::ButtonVariant::Contrast)
                    .on_press(|app: &mut App| submit_rename(app))
                    .build(&kit),
            ),
    );
    Some(
        rune_kit::Dialog::new("settings/rename-device-dialog", "Rename device")
            .open(true)
            .width(420.0)
            .fit()
            .on_close(|app: &mut App| cancel_rename(app))
            .child(body),
    )
}

fn delete_dialog(page: &DevicesPage, app: &App, theme: &Theme) -> Option<rune_kit::Dialog> {
    let device_id = page.delete_confirm.as_ref()?;
    let name = app
        .device_name(device_id)
        .unwrap_or("this device")
        .to_string();
    let copy = format!(
        "Remove “{name}” from the device list? Sessions hosted there stay until you remove them. It will reappear if that machine reconnects."
    );
    let kit = crate::shell::shell_kit(theme);
    let body = row()
        .w_full()
        .justify_end()
        .gap(8.0)
        .child(
            rune_kit::Button::new("Cancel")
                .variant(rune_kit::ButtonVariant::Ghost)
                .on_press(|app: &mut App| cancel_delete(app))
                .build(&kit),
        )
        .child(
            rune_kit::Button::new("Remove")
                .variant(rune_kit::ButtonVariant::Danger)
                .on_press(|app: &mut App| submit_delete(app))
                .build(&kit),
        );
    Some(
        rune_kit::Dialog::new("settings/delete-device-dialog", "Remove device?")
            .open(true)
            .width(440.0)
            .fit()
            .description(copy)
            .on_close(|app: &mut App| cancel_delete(app))
            .child(body),
    )
}

fn device_row(page: &DevicesPage, app: &App, ix: usize, device: &Device, theme: &Theme) -> El {
    let now = Utc::now();
    let online = device_online(device.last_seen_at, now);
    let is_local = app.local_device_id.as_deref() == Some(device.id.as_str());
    let id_copied = page.copied.as_deref() == Some(device.id.as_str());

    // Presence lives ON the identity tile: a corner dot ringed by the card tone.
    let tile = widgets::row_tile(theme, platform_icon(&device.platform)).child(
        el().absolute(30.0, 30.0)
            .w(9.0)
            .h(9.0)
            .radius(5.0)
            .border(2.0, theme.surface)
            .fill(if online {
                theme.success
            } else {
                theme.ink(0.22)
            }),
    );

    let mut meta: Vec<El> = vec![widgets::meta_text(
        theme,
        platform_label(&device.platform).to_string(),
    )];
    if let Some(version) = device.version.as_deref().filter(|v| !v.is_empty()) {
        meta.push(widgets::meta_text(theme, format!("v{version}")));
    }
    if !online {
        meta.push(widgets::meta_text(
            theme,
            format!("Last seen {}", format_last_seen(device.last_seen_at, now)),
        ));
    }
    if let Some(created) = device.created_at {
        meta.push(widgets::meta_text(
            theme,
            format!("Added {}", format_last_seen(Some(created), now)),
        ));
    }
    let copy_id_value = device.id.clone();
    meta.push(
        text(if id_copied {
            "Copied".to_string()
        } else {
            short_id(&device.id)
        })
        .font(TextStyle::new(10.5, 400.0).mono())
        .color(if id_copied {
            theme.success_muted.alpha(0.9)
        } else {
            theme.text_muted.alpha(0.5)
        })
        .cursor(rune::Cursor::Pointer)
        .hover(theme.ink(0.04))
        .on_press_tx(move |app: &mut App, tx: &mut Tx| copy_id(app, tx, copy_id_value.clone())),
    );

    let rename_id = device.id.clone();
    let rename_name = device.name.clone();
    let delete_id = device.id.clone();
    let scope = app.workspace_scope;

    let mut row = widgets::card_row(theme, ix == 0).child(tile).child(
        col()
            .grow(1.0)
            .min_w(0.0)
            .child(widgets::row_title(theme, device.name.clone()))
            .child(widgets::meta_line(theme, meta)),
    );
    if is_local {
        row = row.child(
            text(if scope == Some(WorkspaceScope::Local) {
                "Local only"
            } else {
                "This device"
            })
            .font(TextStyle::new(10.5, 400.0))
            .color(theme.text_muted),
        );
    }
    row = row.child(
        widgets::ghost_action(theme)
            .opacity(0.7)
            .on_press(move |app: &mut App| open_rename(app, rename_id.clone(), rename_name.clone()))
            .child(icons::icon(&icons::PEN, 14.0).color(theme.text_muted))
            .child(
                text("Rename")
                    .font(TextStyle::new(12.0, 400.0))
                    .color(theme.text_muted),
            ),
    );
    if !is_local {
        row = row.child(
            widgets::ghost_action(theme)
                .opacity(0.7)
                .hover(theme.danger.alpha(0.08))
                .on_press(move |app: &mut App| request_delete(app, delete_id.clone()))
                .child(icons::icon(&icons::TRASH_BIN_MINIMALISTIC, 14.0).color(theme.text_muted))
                .child(
                    text("Remove")
                        .font(TextStyle::new(12.0, 400.0))
                        .color(theme.text_muted),
                ),
        );
    }
    row
}

/// The Devices page body.
pub fn render(page: &DevicesPage, app: &App, theme: &Theme) -> El {
    let devices: Vec<Device> = app.devices.clone();
    let count = devices.len();

    let mut card = widgets::section_card(theme);
    if devices.is_empty() {
        card = card.child(
            col().w_full().py(40.0).items_center().child(
                text("No devices registered")
                    .font(TextStyle::new(14.0, 400.0))
                    .color(theme.text_muted.alpha(0.6)),
            ),
        );
    } else {
        for (ix, device) in devices.iter().enumerate() {
            card = card.child(device_row(page, app, ix, device, theme));
        }
    }

    let mut body = widgets::page_column()
        .child(widgets::page_header(
            theme,
            "Devices",
            (count > 0).then_some(count),
        ))
        .child(widgets::page_subtitle(
            theme,
            devices_subtitle(app.workspace_scope),
        ));
    if let Some(message) = page.error.clone() {
        body = body.child(
            widgets::error_strip(theme, message)
                .cursor(rune::Cursor::Pointer)
                .on_press(|app: &mut App| dismiss_error(app)),
        );
    }
    body = body.child(card);

    let mut layer: Vec<El> = Vec::new();
    if let Some(dialog) = rename_dialog(page, theme) {
        dialog.build(&crate::shell::shell_kit(theme), &mut layer);
    }
    if let Some(dialog) = delete_dialog(page, app, theme) {
        dialog.build(&crate::shell::shell_kit(theme), &mut layer);
    }

    rune_kit::root(widgets::page_root("devices-page").child(body), layer)
}

/// Kept for parity with the other pages: the page has no key handling of its own.
pub fn on_key(_app: &mut App, _ev: &KeyEvent) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn presence_window() {
        let now = Utc::now();
        assert!(device_online(Some(now - TimeDelta::seconds(10)), now));
        assert!(device_online(Some(now - TimeDelta::seconds(70)), now));
        assert!(!device_online(Some(now - TimeDelta::seconds(71)), now));
        assert!(!device_online(None, now));
        // Clock skew (future) counts as online.
        assert!(device_online(Some(now + TimeDelta::seconds(30)), now));
    }

    #[test]
    fn last_seen_formatting() {
        let now = Utc::now();
        assert_eq!(format_last_seen(None, now), "never seen");
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::seconds(30)), now),
            "just now"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::hours(3)), now),
            "3h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::days(2)), now),
            "2d ago"
        );
    }

    #[test]
    fn local_subtitle_does_not_claim_synced_metadata() {
        let copy = devices_subtitle(Some(WorkspaceScope::Local));
        assert!(copy.contains("local workspace"));
        assert!(!copy.contains("synced"));
    }

    #[test]
    fn version_skew_errors_say_the_engine_is_old() {
        for detail in [
            "bad params: unknown variant `deleteDevice`, expected one of `createChat`, `createSpace`",
            "bad params: unknown field `deviceId`, expected one of `chatId`, `spaceId`",
            "unknown operation: deleteDevice",
        ] {
            let message = device_mutation_error("Remove", detail);
            assert!(
                message.starts_with("Remove failed: the running engine is older than this app"),
                "got: {message}"
            );
            assert!(
                message.contains("restart or update the engine"),
                "got: {message}"
            );
            assert!(
                message.contains(detail),
                "raw detail must survive: {message}"
            );
        }
    }

    #[test]
    fn ordinary_mutation_errors_keep_the_engine_text_only() {
        let message =
            device_mutation_error("Remove", "cannot remove the local device from the registry");
        assert_eq!(
            message,
            "Remove failed: cannot remove the local device from the registry"
        );
        assert!(!message.contains("older than this app"));
        assert_eq!(
            device_mutation_error("Rename", "transport: connection closed"),
            "Rename failed: transport: connection closed"
        );
    }

    #[test]
    fn short_ids_keep_both_ends() {
        assert_eq!(short_id("abcd1234wxyz"), "abcd1234wxyz");
        assert_eq!(short_id("abcd1234efghijklwxyz"), "abcd1234…wxyz");
    }

    #[test]
    fn platform_labels_match_hearth() {
        assert_eq!(platform_label("darwin"), "macOS");
        assert_eq!(platform_label("linux"), "Linux");
        assert_eq!(platform_label("something"), "something");
    }
}
