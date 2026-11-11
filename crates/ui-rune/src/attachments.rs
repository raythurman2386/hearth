//! Attachments: the plain-text attachment-ref transport that rides a prompt,
//! the transcript read-back cache, chunked upload and the full-size lightbox.
//!
//! Ported from the gpui UI's `attachments.rs`. The cache now stores Rune
//! [`rune::Image`] handles (decoded on Rune's image worker) instead of gpui
//! `Image` assets; `El::pixels` draws them and Rune wakes the frame when a
//! pending decode lands.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rune::{El, SurfaceFit, TextStyle, col, el, text};

use crate::engine::EngineHandle;
use crate::theme::Theme;
use hearth_rpc::methods;

/// `MAX_ATTACHMENT_BYTES` (use-attachments.ts).
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Base64 chars per `UploadChunk`, sized against the relay's 1 MiB frame cap.
pub const UPLOAD_CHUNK_B64_CHARS: usize = 680_000;
/// `MAX_ATTACHMENT_READ_CHUNKS` — bounds the read-back loop.
const MAX_READ_CHUNKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

pub const ATTACHMENT_ONLY_TEXT: &str = "See the attached image(s).";

/// How attachments ride the prompt: plain local paths appended to the text.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let refs: Vec<String> = paths.iter().map(|path| format!("- {path}")).collect();
    let body = if text.is_empty() {
        ATTACHMENT_ONLY_TEXT
    } else {
        text
    };
    format!(
        "{body}\n\nAttached images (local files — open them to view):\n{}",
        refs.join("\n")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImageAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUserMessage {
    /// The visible prompt (the refs trailer stripped; empty for image-only sends).
    pub text: String,
    pub attachments: Vec<UserImageAttachment>,
}

fn name_from_path(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() {
        "image".to_string()
    } else {
        name.to_string()
    }
}

fn find_refs_marker(content: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = "\n\nattached images (local files";
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(needle) {
        let gap = from + rel;
        let line_start = gap + 2;
        let line_end = content[line_start..]
            .find('\n')
            .map(|at| line_start + at)
            .unwrap_or(content.len());
        let line = content[line_start..line_end].trim_end_matches('\r');
        if line.ends_with("):") {
            let refs_start = (line_end + 1).min(content.len());
            return Some((gap, refs_start));
        }
        from = line_start;
    }
    None
}

/// `parseUserMessageImages`: split the visible prompt from its refs trailer.
pub fn parse_user_message_images(content: &str) -> ParsedUserMessage {
    let Some((body_end, refs_start)) = find_refs_marker(content) else {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    };
    let body = content[..body_end].trim_end();
    let attachments: Vec<UserImageAttachment> = content[refs_start..]
        .lines()
        .filter_map(|line| {
            let path = line.trim_start().strip_prefix("- ")?.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .enumerate()
        .map(|(index, path)| UserImageAttachment {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            path,
        })
        .collect();
    if attachments.is_empty() {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments,
        };
    }
    ParsedUserMessage {
        text: if body.trim() == ATTACHMENT_ONLY_TEXT {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
    }
}

/// `userMessageRailText`: what the rail/sidebar shows for a user message.
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_images(content);
    if !parsed.text.trim().is_empty() {
        return parsed.text;
    }
    match parsed.attachments.len() {
        0 => content.to_string(),
        1 => "Attached image".to_string(),
        n => format!("{n} attached images"),
    }
}

// ---------------------------------------------------------------------------
// Staging (use-attachments.ts intake)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StagedAttachment {
    pub id: String,
    /// File name with a type-matching extension.
    pub name: String,
    pub bytes: Arc<[u8]>,
}

impl StagedAttachment {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Image formats the whole pipeline supports.
pub fn format_by_extension(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "svg" => Some("svg"),
        "bmp" => Some("bmp"),
        "tif" | "tiff" => Some("tiff"),
        _ => None,
    }
}

/// `ensureExtension`: pasted screenshots often arrive as a bare "image".
pub fn ensure_extension(name: &str, extension: &str) -> String {
    let has_ext = name
        .rsplit_once('.')
        .map(|(stem, ext)| {
            !stem.is_empty()
                && (2..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or(false);
    if has_ext {
        name.to_string()
    } else {
        format!("{name}.{extension}")
    }
}

/// Stage a file from disk. `Err` carries the user-facing message.
pub fn stage_file(path: &Path) -> Result<StagedAttachment, String> {
    let display_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let Some(format) = format_by_extension(path) else {
        return Err(format!("{display_name} is not a supported image."));
    };
    let meta = std::fs::metadata(path).map_err(|_| format!("{display_name} could not be read."))?;
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!("{display_name} is too large (24 MB max)."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{display_name} could not be read."))?;
    Ok(StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension(&display_name, format),
        bytes: Arc::from(bytes),
    })
}

// ---------------------------------------------------------------------------
// Upload + read-back
// ---------------------------------------------------------------------------

fn with_target(mut params: serde_json::Value, target_device_id: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(map)) = (target_device_id, params.as_object_mut()) {
        map.insert("targetDeviceId".into(), target.into());
    }
    params
}

const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(90);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(150);
const READ_CHUNK_TIMEOUT: Duration = Duration::from_secs(20);

/// Race an RPC against `timeout`.
pub async fn call_with_timeout(
    engine: &EngineHandle,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    match tokio::time::timeout(timeout, engine.client.call(method, params)).await {
        Ok(result) => result.map_err(|err| err.to_string()),
        Err(_) => Err(format!("{method} timed out")),
    }
}

/// Whole-attachment deadline (`120 + 15 × chunks`, capped at 900 s).
fn attachment_deadline(n_chunks: usize) -> Duration {
    Duration::from_secs((120 + 15 * n_chunks as u64).min(900))
}

/// The `(seq, b64 byte-range)` plan for a file's chunks.
fn chunk_ranges(b64_len: usize) -> Vec<(u64, std::ops::Range<usize>)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut seq = 0u64;
    loop {
        let end = (start + UPLOAD_CHUNK_B64_CHARS).min(b64_len);
        ranges.push((seq, start..end));
        start = end;
        seq += 1;
        if start >= b64_len {
            break;
        }
    }
    ranges
}

/// Chunked upload: base64 the bytes, `UploadChunk` per slice, then
/// `UploadCommit` → the durable absolute path on the target device.
pub async fn upload_attachment(
    engine: &EngineHandle,
    target_device_id: Option<&str>,
    upload_id: &str,
    attachment: &StagedAttachment,
    progress: Option<Arc<std::sync::atomic::AtomicU64>>,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let ranges = chunk_ranges(b64.len());
    let upload = async {
        for (seq, range) in ranges.iter().cloned() {
            let params = with_target(
                serde_json::json!({
                    "uploadId": upload_id,
                    "seq": seq,
                    "data": &b64[range.clone()],
                }),
                target_device_id,
            );
            let timeout = if seq < 3 {
                FIRST_CHUNK_TIMEOUT
            } else {
                CHUNK_TIMEOUT
            };
            let mut attempt = 0u32;
            loop {
                match call_with_timeout(engine, methods::UPLOAD_CHUNK, params.clone(), timeout)
                    .await
                {
                    Ok(_) => break,
                    Err(err) if attempt < 2 => {
                        attempt += 1;
                        tracing::warn!(error = %err, seq, attempt, "upload chunk retry");
                        tokio::time::sleep(Duration::from_millis(50 * attempt as u64 * (seq + 1)))
                            .await;
                    }
                    Err(err) => return Err(err),
                }
            }
            if let Some(progress) = &progress {
                progress.fetch_add(
                    (range.len() * 3 / 4) as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "fileName": attachment.name }),
            target_device_id,
        );
        let reply =
            call_with_timeout(engine, methods::UPLOAD_COMMIT, params, COMMIT_TIMEOUT).await?;
        reply
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| "upload commit returned no path".to_string())
    };
    match tokio::time::timeout(attachment_deadline(ranges.len()), upload).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "attachment upload exceeded {}s",
            attachment_deadline(ranges.len()).as_secs()
        )),
    }
}

pub struct LoadedAttachmentImage {
    pub name: String,
    pub image: Arc<rune::Image>,
}

/// Rune's content-addressed image cache. Decoding runs on its own worker; the
/// no-op waker is enough because a drawn `Pixels` handle wakes the window that
/// shows it when the decode lands.
fn image_cache() -> &'static rune::ImageCache {
    static CACHE: OnceLock<rune::ImageCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let wake: rune::Waker = Arc::new(|| {});
        rune::ImageCache::new(wake)
    })
}

/// `ReadAttachmentChunk` loop until `done`.
pub async fn read_attachment_image(
    engine: &EngineHandle,
    target_device_id: Option<&str>,
    path: &str,
) -> Option<LoadedAttachmentImage> {
    let mut name = String::new();
    let mut b64 = String::new();
    let mut offset = 0u64;
    let mut done = false;
    for _ in 0..MAX_READ_CHUNKS {
        let params = with_target(
            serde_json::json!({ "path": path, "offset": offset }),
            target_device_id,
        );
        let chunk = call_with_timeout(
            engine,
            methods::READ_ATTACHMENT_CHUNK,
            params,
            READ_CHUNK_TIMEOUT,
        )
        .await
        .ok()?;
        name = chunk.get("name")?.as_str()?.to_string();
        b64.push_str(chunk.get("data")?.as_str()?);
        done = chunk.get("done")?.as_bool()?;
        if done {
            break;
        }
        let next = chunk.get("nextOffset")?.as_u64()?;
        if next <= offset {
            return None;
        }
        offset = next;
    }
    if !done || b64.is_empty() {
        return None;
    }
    let bytes = BASE64.decode(b64.as_bytes()).ok()?;
    let image = image_cache().load(bytes);
    Some(LoadedAttachmentImage {
        name: if name.is_empty() {
            name_from_path(path)
        } else {
            name
        },
        image: Arc::new(image),
    })
}

// ---------------------------------------------------------------------------
// Transcript image cache (transcript-attachment-cache.ts)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CachedAttachmentImage {
    pub name: String,
    pub image: Arc<rune::Image>,
}

/// What a render pass sees for one `(deviceId, path)` source.
#[derive(Clone)]
pub enum AttachmentSnapshot {
    Loading,
    Loaded(CachedAttachmentImage),
    /// Load failed; `retry_in` is how long until [`begin_load`] would hand out
    /// another attempt (the 2s→15s ladder).
    Error {
        retry_in: Duration,
    },
}

enum CacheEntry {
    Loading {
        attempts: u32,
    },
    Loaded {
        image: CachedAttachmentImage,
        last_used: u64,
    },
    Error {
        attempts: u32,
        at: Instant,
    },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

/// Cap on retained decoded images. Rune's `ImageCache` already dedupes by
/// content; this bound keeps this map (and therefore its live handles) finite.
const MAX_CACHED_IMAGES: usize = 128;

#[derive(Default)]
struct ImageCache {
    map: HashMap<(String, String), CacheEntry>,
    tick: u64,
}

impl ImageCache {
    fn insert_loaded(&mut self, key: (String, String), image: CachedAttachmentImage) {
        self.tick += 1;
        self.map.insert(
            key,
            CacheEntry::Loaded {
                image,
                last_used: self.tick,
            },
        );
        while self.map.len() > MAX_CACHED_IMAGES {
            let oldest = self
                .map
                .iter()
                .filter(|(_, entry)| matches!(entry, CacheEntry::Loaded { .. }))
                .filter_map(|(key, entry)| match entry {
                    CacheEntry::Loaded { last_used, .. } => Some((*last_used, key.clone())),
                    _ => None,
                })
                .min();
            let Some((_, victim)) = oldest else { break };
            self.map.remove(&victim);
        }
    }
}

fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

fn key(device_id: &str, path: &str) -> (String, String) {
    (device_id.to_string(), path.to_string())
}

pub fn attachment_snapshot(device_id: &str, path: &str) -> AttachmentSnapshot {
    let mut cache = cache().lock().unwrap();
    let tick = {
        cache.tick += 1;
        cache.tick
    };
    match cache.map.get_mut(&key(device_id, path)) {
        Some(CacheEntry::Loaded { image, last_used }) => {
            *last_used = tick;
            AttachmentSnapshot::Loaded(image.clone())
        }
        Some(CacheEntry::Error { attempts, at }) => AttachmentSnapshot::Error {
            retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
        },
        Some(CacheEntry::Loading { .. }) => AttachmentSnapshot::Loading,
        None => {
            // Queued-send alias: a committed upload's basename starts with the
            // upload id's first 8 chars (`{id8}-{name}`).
            if let Some(image) = upload_alias_id8(path).and_then(|id8| {
                match cache.map.get(&alias_key(device_id, &id8)) {
                    Some(CacheEntry::Loaded { image, .. }) => Some(image.clone()),
                    _ => None,
                }
            }) {
                cache.insert_loaded(key(device_id, path), image.clone());
                return AttachmentSnapshot::Loaded(image);
            }
            AttachmentSnapshot::Loading
        }
    }
}

fn upload_alias_id8(path: &str) -> Option<String> {
    let base = Path::new(path).file_name()?.to_str()?;
    let (id8, _) = base.split_at_checked(8)?;
    (base.as_bytes().get(8) == Some(&b'-') && id8.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| id8.to_string())
}

fn alias_key(device_id: &str, id8: &str) -> (String, String) {
    key(device_id, &format!("upload-alias://{id8}"))
}

/// Seed the just-sent image under its upload identity.
pub fn seed_attachment_alias(
    device_id: &str,
    upload_id: &str,
    name: &str,
    image: Arc<rune::Image>,
) {
    let id8: String = upload_id.chars().take(8).collect();
    let (device, path) = alias_key(device_id, &id8);
    store_loaded(&device, &path, name, image);
}

/// Claim the load for a source: `true` ⇒ start fetching now.
pub fn begin_load(device_id: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    let entry = cache.map.entry(key(device_id, path));
    match entry {
        std::collections::hash_map::Entry::Vacant(vacant) => {
            vacant.insert(CacheEntry::Loading { attempts: 0 });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut occupied) => match occupied.get() {
            CacheEntry::Error { attempts, at }
                if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) =>
            {
                let attempts = *attempts;
                occupied.insert(CacheEntry::Loading { attempts });
                true
            }
            _ => false,
        },
    }
}

pub fn store_loaded(device_id: &str, path: &str, name: &str, image: Arc<rune::Image>) {
    cache().lock().unwrap().insert_loaded(
        key(device_id, path),
        CachedAttachmentImage {
            name: name.to_string(),
            image,
        },
    );
}

pub fn store_error(device_id: &str, path: &str) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.map.get(&key(device_id, path)) {
        Some(CacheEntry::Loading { attempts }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        key(device_id, path),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
        },
    );
}

/// Seed the cache after a successful upload.
pub fn seed_attachment(device_id: &str, path: &str, name: &str, image: Arc<rune::Image>) {
    store_loaded(device_id, path, name, image);
}

// ---------------------------------------------------------------------------
// Preview lightbox
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PreviewImage {
    pub name: String,
    pub image: Arc<rune::Image>,
}

/// The bare lightbox: dim scrim, the image at ≤85% height / 90% width, the
/// file name under it. Any click closes.
pub fn lightbox(
    preview: &PreviewImage,
    theme: &Theme,
    on_close: impl Fn(&mut crate::state::App) + 'static,
) -> El {
    el().key("app/transcript/attachment-lightbox")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .fill(theme.scrim())
        .center()
        .cursor(rune::Cursor::Pointer)
        .on_press(move |app: &mut crate::state::App| on_close(app))
        .child(
            col()
                .gap(12.0)
                .center()
                .child(
                    el().max_w(0.9 * crate::app::WINDOW_SIZE.0)
                        .max_h(0.85 * crate::app::WINDOW_SIZE.1)
                        .radius(6.0)
                        .clip_rounded(6.0)
                        .pixels_fit(preview.image.pixels(), SurfaceFit::Fit),
                )
                .child(
                    text(preview.name.clone())
                        .font(TextStyle::new(11.0, 400.0))
                        .color(theme.text.alpha(0.45))
                        .truncate()
                        .max_w(0.9 * crate::app::WINDOW_SIZE.0),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn with_attachments_round_trips_through_parse() {
        let paths = vec!["/data/uploads/ab-cat.png".to_string(), "/x/dog.jpg".into()];
        let content = with_attachments("look at these", &paths);
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "look at these");
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].path, "/data/uploads/ab-cat.png");
        assert_eq!(parsed.attachments[0].name, "ab-cat.png");
        assert_eq!(parsed.attachments[1].name, "dog.jpg");
        assert_eq!(parsed.attachments[0].id, "0:/data/uploads/ab-cat.png");
    }

    #[test]
    fn image_only_send_hides_placeholder_body() {
        let content = with_attachments("", &["/a/b.png".to_string()]);
        assert!(content.starts_with(ATTACHMENT_ONLY_TEXT));
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(with_attachments("hello", &[]), "hello");
        let parsed = parse_user_message_images("hello\n\nno images here");
        assert!(parsed.attachments.is_empty());
        assert_eq!(parsed.text, "hello\n\nno images here");
    }

    #[test]
    fn marker_is_case_insensitive_and_requires_ref_lines() {
        let parsed = parse_user_message_images(
            "hi\n\nATTACHED IMAGES (local files — open them to view):\n- /p/q.png",
        );
        assert_eq!(parsed.attachments.len(), 1);
        let empty = parse_user_message_images(
            "hi\n\nAttached images (local files — open them to view):\nnothing",
        );
        assert!(empty.attachments.is_empty());
        assert!(empty.text.contains("Attached images"));
    }

    #[test]
    fn rail_text_summarizes_image_only_sends() {
        let one = with_attachments("", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&one), "Attached image");
        let two = with_attachments("", &["/a/b.png".to_string(), "/c/d.png".into()]);
        assert_eq!(user_message_rail_text(&two), "2 attached images");
        let with_text = with_attachments("fix this", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&with_text), "fix this");
        assert_eq!(user_message_rail_text("plain"), "plain");
    }

    #[test]
    fn ensure_extension_matches_browser_heuristic() {
        assert_eq!(ensure_extension("shot.png", "png"), "shot.png");
        assert_eq!(ensure_extension("image", "png"), "image.png");
        assert_eq!(ensure_extension("photo.j", "jpg"), "photo.j.jpg");
        assert_eq!(ensure_extension("archive.tar.gz", "png"), "archive.tar.gz");
    }

    #[test]
    fn supported_formats_match_engine_jail() {
        for (ext, expect) in [
            ("png", Some("png")),
            ("JPG", Some("jpg")),
            ("webp", Some("webp")),
            ("svg", Some("svg")),
            ("ico", None),
            ("txt", None),
        ] {
            assert_eq!(
                format_by_extension(Path::new(&format!("f.{ext}"))),
                expect,
                "ext {ext}"
            );
        }
    }

    #[test]
    fn retry_ladder_is_2s_doubling_capped_at_15s() {
        assert_eq!(retry_delay(0), Duration::from_millis(2_000));
        assert_eq!(retry_delay(1), Duration::from_millis(4_000));
        assert_eq!(retry_delay(2), Duration::from_millis(8_000));
        assert_eq!(retry_delay(3), Duration::from_millis(15_000));
        assert_eq!(retry_delay(9), Duration::from_millis(15_000));
    }

    #[test]
    fn upload_chunk_fits_the_relay_frame_ceiling() {
        const { assert!(UPLOAD_CHUNK_B64_CHARS + 1_024 < 1_048_576) };
        assert_eq!(UPLOAD_CHUNK_B64_CHARS % 4, 0);
    }

    #[test]
    fn chunk_ranges_cover_the_buffer_exactly() {
        assert_eq!(chunk_ranges(0), vec![(0, 0..0)]);
        let exact = chunk_ranges(UPLOAD_CHUNK_B64_CHARS * 2);
        assert_eq!(exact.len(), 2);
        assert_eq!(
            exact[1],
            (1, UPLOAD_CHUNK_B64_CHARS..UPLOAD_CHUNK_B64_CHARS * 2)
        );
        let partial = chunk_ranges(UPLOAD_CHUNK_B64_CHARS + 7);
        assert_eq!(partial.len(), 2);
        let mut expected_start = 0;
        for (seq, range) in &partial {
            assert_eq!(range.start, expected_start, "seq {seq} contiguous");
            expected_start = range.end;
        }
        assert_eq!(expected_start, UPLOAD_CHUNK_B64_CHARS + 7);
    }

    #[test]
    fn attachment_deadline_scales_and_caps() {
        assert_eq!(attachment_deadline(1), Duration::from_secs(135));
        assert_eq!(attachment_deadline(1_000), Duration::from_secs(900));
    }

    #[test]
    fn upload_alias_recognises_committed_basenames() {
        assert_eq!(
            upload_alias_id8("/uploads/abcd1234-shot.png").as_deref(),
            Some("abcd1234")
        );
        assert_eq!(upload_alias_id8("/uploads/shot.png"), None);
        assert_eq!(upload_alias_id8("/uploads/abcd12345-shot.png"), None);
    }
}
