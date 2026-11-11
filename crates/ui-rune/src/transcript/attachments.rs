//! User-bubble attachment strips.
//!
//! Fixed 112×80 thumbnails in a strip that never changes height, so a load
//! state flip cannot shift the virtualizer. The read-back cache itself lives in
//! [`crate::attachments`]; this module resolves a path across its candidate
//! devices, starts the load through the bridge, and draws the result with Rune
//! `El::pixels`.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rune::{El, SurfaceFit, el, row};

use crate::attachments::{
    AttachmentSnapshot, UserImageAttachment, attachment_snapshot, begin_load,
    read_attachment_image, store_error, store_loaded,
};
use crate::state::App;
use crate::theme::Theme;

pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;

/// Devices that may own a user message's attachment files: the chat's host
/// device plus this device.
pub fn attachment_device_ids(app: &App) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(chat_id) = app.selected_chat.as_deref()
        && let Some(chat) = app.chats.iter().find(|chat| chat.id == chat_id)
    {
        ids.push(chat.device_id.clone());
    }
    if let Some(local) = app.local_device_id.clone()
        && !ids.contains(&local)
    {
        ids.push(local);
    }
    ids
}

fn retry_scheduled() -> &'static Mutex<std::collections::HashSet<(String, String)>> {
    static SET: OnceLock<Mutex<std::collections::HashSet<(String, String)>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn spawn_load(app: &App, device_id: String, path: String, retry_in: Option<Duration>) {
    let Some(engine) = app.engine.clone() else {
        store_error(&device_id, &path);
        return;
    };
    let bridge = app.bridge.clone();
    let spawn_bridge = bridge.clone();
    let local = app.local_device_id.clone();
    let target = (local.as_deref() != Some(device_id.as_str())).then(|| device_id.clone());
    spawn_bridge.spawn(async move {
        if let Some(delay) = retry_in {
            tokio::time::sleep(delay + Duration::from_millis(60)).await;
            retry_scheduled()
                .lock()
                .unwrap()
                .remove(&(device_id.clone(), path.clone()));
        }
        match read_attachment_image(&engine, target.as_deref(), &path).await {
            Some(loaded) => store_loaded(&device_id, &path, &loaded.name, loaded.image),
            None => store_error(&device_id, &path),
        }
        bridge.wake();
    });
}

/// Effective load state for one attachment across its candidate devices: first
/// Loaded source wins; otherwise loads are (re)claimed and the snapshot
/// degrades Loading → Error with a scheduled retry wake-up.
fn attachment_state(app: &App, device_ids: &[String], path: &str) -> AttachmentSnapshot {
    for device in device_ids {
        if let AttachmentSnapshot::Loaded(image) = attachment_snapshot(device, path) {
            return AttachmentSnapshot::Loaded(image);
        }
    }
    let mut any_loading = false;
    let mut min_retry: Option<Duration> = None;
    for device in device_ids {
        if begin_load(device, path) {
            spawn_load(app, device.clone(), path.to_string(), None);
        }
        match attachment_snapshot(device, path) {
            AttachmentSnapshot::Loaded(image) => return AttachmentSnapshot::Loaded(image),
            AttachmentSnapshot::Loading => any_loading = true,
            AttachmentSnapshot::Error { retry_in } => {
                min_retry = Some(min_retry.map_or(retry_in, |min| min.min(retry_in)));
            }
        }
    }
    if any_loading {
        return AttachmentSnapshot::Loading;
    }
    match min_retry {
        Some(retry_in) => {
            if let Some(device) = device_ids.first() {
                let key = (device.clone(), path.to_string());
                if retry_in != Duration::MAX
                    && retry_scheduled().lock().unwrap().insert(key.clone())
                {
                    spawn_load(app, device.clone(), path.to_string(), Some(retry_in));
                }
            }
            AttachmentSnapshot::Error { retry_in }
        }
        None => AttachmentSnapshot::Error {
            retry_in: Duration::MAX,
        },
    }
}

/// The right-aligned thumbnail strip above a user bubble.
pub fn render_user_attachments(
    app: &App,
    row_id: &str,
    attachments: &[UserImageAttachment],
    theme: &Theme,
) -> El {
    let device_ids = attachment_device_ids(app);
    let mut strip = row()
        .w_full()
        .h(ATT_STRIP_H)
        .justify_end()
        .items_start()
        .gap(8.0)
        .px(4.0)
        .pt(4.0);
    for (ix, attachment) in attachments.iter().enumerate() {
        let state = attachment_state(app, &device_ids, &attachment.path);
        let frame = el()
            .size(ATT_THUMB_W, ATT_THUMB_H)
            .radius(8.0)
            .clip_rounded(8.0);
        let thumb = match state {
            AttachmentSnapshot::Loaded(image) => {
                let preview = crate::attachments::PreviewImage {
                    name: image.name.clone(),
                    image: image.image.clone(),
                };
                frame
                    .key(format!("{row_id}#att{ix}"))
                    .border(1.0, theme.hairline(0.11))
                    .fill(theme.ink(0.035))
                    .cursor(rune::Cursor::Pointer)
                    .on_press(move |app: &mut App| {
                        app.transcript_state.attachment_preview = Some(preview.clone());
                    })
                    .child(
                        el().size(ATT_THUMB_W - 2.0, ATT_THUMB_H - 2.0)
                            .radius(7.0)
                            .clip_rounded(7.0)
                            .pixels_fit(image.image.pixels(), SurfaceFit::Fill),
                    )
            }
            AttachmentSnapshot::Error { .. } => frame
                .border(1.0, theme.hairline(0.14))
                .fill(theme.ink(0.025)),
            AttachmentSnapshot::Loading => frame
                .border(1.0, theme.hairline(0.08))
                .fill(theme.ink(0.055))
                .opacity(0.55),
        };
        strip = strip.child(thumb);
    }
    strip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_keeps_a_fixed_height() {
        assert_eq!(ATT_STRIP_H, 90.0);
        assert_eq!(ATT_THUMB_W, 112.0);
        assert_eq!(ATT_THUMB_H, 80.0);
    }
}
