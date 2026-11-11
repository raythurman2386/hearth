//! The space and device pickers: their rows, ordering and the picks that
//! re-key the draft.

use hearth_proto::Device;

use crate::state::App;

use super::PickersState;

/// The picker's project rows: scoped to the canvas's device — the device
/// switcher narrows the list, projects on other devices don't show (pick the
/// device first, then its project). Unscoped only while the device is still
/// unknown (pre-probe boot).
pub(crate) fn scoped_space_rows(state: &PickersState, app: &App) -> Vec<hearth_proto::Space> {
    let _ = state;
    let device = app.effective_device_id();
    app.spaces_sorted()
        .into_iter()
        .filter(|s| match device.as_deref() {
            Some(d) => s.device_id == d,
            None => true,
        })
        .cloned()
        .collect()
}

/// Re-home the canvas onto another project. The owner sync in
/// [`super::pump`] does the heavy lifting: branch draft, ref cache, and the
/// per-device harness/model catalogs all invalidate on the project change.
pub fn pick_space(app: &mut App, space_id: String) {
    app.select_space(Some(space_id));
    remember_target(app);
    app.pickers.open = None;
}

pub fn pick_device(app: &mut App, device_id: String) {
    app.select_device(Some(device_id));
    remember_target(app);
    app.pickers.open = None;
}

/// Persist the device/project picks — the "last selected" defaults the next
/// boot's canvas restores.
fn remember_target(app: &mut App) {
    app.pickers.defaults.device = app
        .selected_device
        .clone()
        .or_else(|| app.local_device_id.clone());
    app.pickers.defaults.project = app.selected_space.clone();
    app.pickers.defaults.no_project = app.no_project;
    app.pickers.save_defaults();
}

/// Devices in picker order: this device first, then by name.
pub fn device_rows(app: &App) -> Vec<Device> {
    ordered_devices(&app.devices, app.local_device_id.as_deref())
}

/// Pure ordering used by [`device_rows`] (and tested without an `App`).
pub(crate) fn ordered_devices(devices: &[Device], local: Option<&str>) -> Vec<Device> {
    let mut devices: Vec<Device> = devices.to_vec();
    devices.sort_by_key(|d| {
        (
            local != Some(d.id.as_str()),
            d.name.to_lowercase(),
            d.id.clone(),
        )
    });
    devices
}

/// The project popover's "New project…" row: close the picker and hand the
/// request to the shell (which owns its add-space palette).
pub fn request_new_project(app: &mut App) {
    app.pickers.open = None;
    app.pickers.new_project_requested = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn devices_order_local_first_then_by_name() {
        let devices = vec![
            device("b", "zulu"),
            device("a", "alpha"),
            device("local", "workstation"),
        ];
        let rows = ordered_devices(&devices, Some("local"));
        assert_eq!(
            rows.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["local", "a", "b"]
        );
        // No local device known: pure name order (alpha, workstation, zulu),
        // ids break name ties.
        let rows = ordered_devices(&devices, None);
        assert_eq!(
            rows.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "local", "b"]
        );
    }
}
