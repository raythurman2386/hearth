//! Staged-attachment state, the text transport that rides the prompt, the
//! chunked upload to the chat's host device, the read-back loop, the strip
//! layout and the full-size preview lightbox.
//!
//! Port of the gpui UI's `attachments.rs` + `composer/attachments.rs`. Rune has
//! no `Image` type that carries raw bytes, so a [`StagedAttachment`] keeps its
//! encoded bytes (fed to `UploadChunk`) and thumbnails come from the module's
//! [`rune::ImageCache`] (see [`thumbnail`]).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use hearth_proto::version_triple;
use hearth_rpc::{RpcClient, methods};

use crate::attachments::AttachmentSnapshot;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Base64 chars per `UploadChunk`, sized against the relay's hard ceiling:
/// Cloudflare caps a WebSocket message at 1 MiB, and a chunk rides one relay
/// frame (JSON envelope + uleb header add ~150 bytes) — 680 000 chars ≈
/// 510 KB binary leaves ~35% headroom. Multiple of 4 so a slice of the
/// whole-file base64 stays independently decodable.
pub const UPLOAD_CHUNK_B64_CHARS: usize = 680_000;
/// state.ts `MAX_ATTACHMENT_READ_CHUNKS` — bounds the read-back loop.
const MAX_READ_CHUNKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

/// Image formats the whole pipeline supports: the intersection of the decoders
/// the UI ships and the engine's `mime_by_ext` read-back jail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Svg,
    Bmp,
    Tiff,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Svg => "svg",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Svg => "image/svg+xml",
            Self::Bmp => "image/bmp",
            Self::Tiff => "image/tiff",
        }
    }

    pub fn from_mime_type(mime: &str) -> Option<Self> {
        match mime.split(';').next()?.trim() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/svg+xml" => Some(Self::Svg),
            "image/bmp" | "image/x-ms-bmp" => Some(Self::Bmp),
            "image/tiff" => Some(Self::Tiff),
            _ => None,
        }
    }
}

/// Image formats the picker accepts, by file extension.
pub fn format_by_extension(path: &Path) -> Option<ImageFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "svg" => Some(ImageFormat::Svg),
        "bmp" => Some(ImageFormat::Bmp),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// use-attachments.ts `ensureExtension`: pasted screenshots often arrive as a
/// bare "image" — make sure the staged name carries a type-matching extension.
pub fn ensure_extension(name: &str, format: ImageFormat) -> String {
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
        format!("{name}.{}", format.extension())
    }
}

// ---------------------------------------------------------------------------
// Staged attachment
// ---------------------------------------------------------------------------

/// An image staged in the composer, before upload. The encoded bytes feed both
/// the thumbnail decode and the chunked upload.
#[derive(Debug, Clone)]
pub struct StagedAttachment {
    pub id: String,
    /// File name with a type-matching extension (agents sniff images by
    /// extension).
    pub name: String,
    pub format: ImageFormat,
    bytes: Arc<[u8]>,
}

impl StagedAttachment {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The encoded bytes as a shared handle. Seeding the transcript cache is a
    /// refcount bump, never another whole-file copy.
    pub fn bytes_arc(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

/// Files read off the UI thread and ready to stage (see [`stage_paths`]).
#[derive(Debug, Default)]
pub struct StagedFiles {
    pub staged: Vec<StagedAttachment>,
    /// One user-facing message per file that could not be read.
    pub errors: Vec<String>,
}

/// Stage every supported path picked/dropped/pasted at once: unsupported
/// extensions are skipped and unreadable files collect an error instead of
/// aborting the batch.
pub fn stage_paths(paths: &[PathBuf]) -> StagedFiles {
    stage_paths_with(paths, stage_file)
}

/// [`stage_paths`] over an injected reader, so the multi-path policy is
/// testable without touching the filesystem.
pub fn stage_paths_with(
    paths: &[PathBuf],
    read: impl Fn(&Path) -> Result<StagedAttachment, String>,
) -> StagedFiles {
    let mut files = StagedFiles::default();
    for path in paths {
        if format_by_extension(path).is_none() {
            continue;
        }
        match read(path) {
            Ok(attachment) => files.staged.push(attachment),
            Err(message) => files.errors.push(message),
        }
    }
    files
}

/// Stage a file from disk (picker / drop / pasted path). `Err` carries the
/// user-facing message.
pub fn stage_file(path: &Path) -> Result<StagedAttachment, String> {
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
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
        format,
        bytes: bytes.into(),
    })
}

/// Stage an image pasted from the clipboard (PNG bytes, as Rune delivers them).
pub fn stage_clipboard_image(png: &[u8]) -> StagedAttachment {
    StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension("image", ImageFormat::Png),
        format: ImageFormat::Png,
        bytes: png.to_vec().into(),
    }
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

thread_local! {
    /// The UI thread's shared decode cache. Rune decodes off-thread and wakes
    /// through the app waker.
    static IMAGE_CACHE: std::cell::RefCell<Option<rune::ImageCache>> =
        const { std::cell::RefCell::new(None) };
}

static WAKER: Mutex<Option<rune::runtime::Waker>> = Mutex::new(None);

/// Give the composer's image cache the app's wake handle (call once from
/// `AppSpec::setup`, beside `bridge.set_waker`). Thumbnails decode on a worker
/// and wake the UI so their frame paints.
pub fn set_waker(waker: rune::runtime::Waker) {
    *WAKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
}

fn with_cache<R>(f: impl FnOnce(&rune::ImageCache) -> R) -> R {
    IMAGE_CACHE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let cache = slot.get_or_insert_with(|| {
            rune::ImageCache::new(Arc::new(|| {
                if let Some(waker) = WAKER.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    waker();
                }
            }))
        });
        f(cache)
    })
}

/// The decoded thumbnail for a staged attachment (deduplicated by content).
pub fn thumbnail(attachment: &StagedAttachment) -> rune::Image {
    with_cache(|cache| cache.load(attachment.bytes.to_vec()))
}

/// A full-size preview target (staged strip or transcript thumbnail).
#[derive(Clone)]
pub struct PreviewImage {
    pub name: String,
    pub image: rune::Image,
}

impl PreviewImage {
    pub fn of(attachment: &StagedAttachment) -> Self {
        Self {
            name: attachment.name.clone(),
            image: thumbnail(attachment),
        }
    }
}

/// The bare lightbox: dim scrim, the image at ≤85vh/90vw, the file name under
/// it. Any click closes, and so does Escape (the element is modal).
pub fn lightbox(
    theme: &Theme,
    preview: &PreviewImage,
    close: impl Fn(&mut crate::state::App) + 'static,
) -> rune::El {
    use rune::{SurfaceFit, TextStyle, col, el, text};
    let close = std::rc::Rc::new(close);
    let click = close.clone();
    let key = close.clone();
    el().key("attachment-lightbox")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .fill(crate::theme::scrim_for(theme.appearance, 0.70))
        .center()
        .cursor(rune::Cursor::Pointer)
        .modal(true, move |app: &mut crate::state::App| (key)(app))
        .on_press(move |app: &mut crate::state::App| (click)(app))
        .on_key(
            move |app: &mut crate::state::App, ev: &rune::KeyEvent, _tx: &mut rune::Tx| {
                if matches!(ev.key, rune::Key::Escape) {
                    (close)(app);
                    true
                } else {
                    false
                }
            },
        )
        .child(
            col()
                .w_full()
                .h_full()
                .padding(64.0)
                .gap(12.0)
                .center()
                .child(
                    el().grow(1.0)
                        .min_h(0.0)
                        .w_full()
                        .radius(6.0)
                        .pixels_fit(preview.image.pixels(), SurfaceFit::Fit),
                )
                .child(
                    text(preview.name.clone())
                        .font(TextStyle::new(11.0, 400.0))
                        .color(theme.ink(0.45))
                        .truncate(),
                ),
        )
}

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

/// The body used for image-only sends (`use-attachments.ts`).
pub const ATTACHMENT_ONLY_TEXT: &str = "See the attached image(s).";

/// How attachments ride the prompt: plain local paths appended to the text —
/// the files are staged on the device that runs the agent, so the agent can
/// open them with its own tools; the same text is what persists as the user
/// doc entry.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let refs: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
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

/// An attachment ref parsed back out of a user message's text.
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

/// Find the refs trailer: a blank line, then a line starting (case-insensitive)
/// with `Attached images (local files` and ending `):`. Returns
/// `(body_end, refs_start)` byte offsets.
fn find_refs_marker(content: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = "\n\nattached images (local files";
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(needle) {
        let gap = from + rel;
        let line_start = gap + 2;
        let line_end = content[line_start..]
            .find('\n')
            .map(|p| line_start + p)
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

/// Split a user message's text into the visible prompt and its attachment refs.
pub fn parse_user_message_images(content: &str) -> ParsedUserMessage {
    let Some((body_end, refs_start)) = find_refs_marker(content) else {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    };
    let mut attachments = Vec::new();
    for line in content[refs_start..].lines() {
        let Some(path) = line.trim().strip_prefix("- ") else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        attachments.push(UserImageAttachment {
            id: format!("{}:{path}", attachments.len()),
            path: path.to_string(),
            name: name_from_path(path),
        });
    }
    if attachments.is_empty() {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    }
    let body = content[..body_end].trim_end();
    ParsedUserMessage {
        text: if body.trim() == ATTACHMENT_ONLY_TEXT {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
    }
}

/// Rail summary for a user message: the prompt, or an image-only count.
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_images(content);
    if !parsed.text.is_empty() {
        return parsed.text;
    }
    match parsed.attachments.len() {
        0 => content.to_string(),
        1 => "Attached image".to_string(),
        n => format!("{n} attached images"),
    }
}

// ---------------------------------------------------------------------------
// Post-send seed cache (transcript reads its own bubbles locally)
// ---------------------------------------------------------------------------

/// Upper bound on the bytes retained by the transcript seed cache. Seeded
/// bytes duplicate what the composer's staged attachments already hold (the
/// `Arc` is shared, so this is a conservative per-entry accounting), and the
/// transcript decodes its own images — the seed only bridges the frames right
/// after a send.
const MAX_SEEDED_BYTES: usize = 64 * 1024 * 1024;
/// Belt and braces: even tiny files can't grow the map without bound.
const MAX_SEEDED_ENTRIES: usize = 32;

/// How long a seed stays readable after the send that created it. A seed that
/// nobody consumed within the budget is retired by the per-frame sweep.
pub const SEED_TTL: Duration = Duration::from_secs(20);
/// Minimum spacing between sweeps: the sweep reads the transcript's image
/// cache, which is too much work for every idle frame.
const SEED_SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Local bytes seeded for a `(device, path)` so the transcript can show the
/// sender's own attachment without a round-trip.
struct Seeded {
    name: String,
    bytes: Arc<[u8]>,
    last_used: Instant,
}

/// Bounded, recency-ordered seed store (the shape of [`crate::syntax_cache`]).
#[derive(Default)]
struct SeedCache {
    entries: HashMap<(String, String), Seeded>,
    recency: VecDeque<(String, String)>,
    retained_bytes: usize,
    last_sweep: Option<Instant>,
}

impl SeedCache {
    /// Insert (or replace) a seed; returns false when a single file exceeds the
    /// whole budget, in which case nothing is retained.
    fn insert(&mut self, device_id: &str, path: &str, name: &str, bytes: Arc<[u8]>) -> bool {
        let key = (device_id.to_string(), path.to_string());
        if let Some(previous) = self.entries.remove(&key) {
            self.retained_bytes = self.retained_bytes.saturating_sub(previous.bytes.len());
            self.recency.retain(|candidate| *candidate != key);
        }
        let len = bytes.len();
        if len > MAX_SEEDED_BYTES {
            return false;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(len);
        self.entries.insert(
            key.clone(),
            Seeded {
                name: name.to_string(),
                bytes,
                last_used: Instant::now(),
            },
        );
        self.recency.push_back(key.clone());
        while self.entries.len() > MAX_SEEDED_ENTRIES || self.retained_bytes > MAX_SEEDED_BYTES {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.retained_bytes = self.retained_bytes.saturating_sub(removed.bytes.len());
            }
        }
        self.entries.contains_key(&key)
    }

    fn get(&mut self, device_id: &str, path: &str) -> Option<(String, Arc<[u8]>)> {
        let key = (device_id.to_string(), path.to_string());
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = Instant::now();
        let value = (entry.name.clone(), entry.bytes.clone());
        self.recency.retain(|candidate| *candidate != key);
        self.recency.push_back(key);
        Some(value)
    }

    fn remove(&mut self, device_id: &str, path: &str) -> bool {
        let key = (device_id.to_string(), path.to_string());
        let Some(removed) = self.entries.remove(&key) else {
            return false;
        };
        self.retained_bytes = self.retained_bytes.saturating_sub(removed.bytes.len());
        self.recency.retain(|candidate| *candidate != key);
        true
    }

    /// Drop every seed untouched since `cutoff`; returns how many went.
    fn prune_before(&mut self, cutoff: Instant) -> usize {
        let expired: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.last_used <= cutoff)
            .map(|(key, _)| key.clone())
            .collect();
        let count = expired.len();
        for key in expired {
            self.remove(&key.0, &key.1);
        }
        count
    }

    /// Periodic sweep: drop seeds whose transcript thumbnail already resolved
    /// (the decoded image is authoritative now) and seeds past [`SEED_TTL`].
    /// Rate-limited so an idle app does not crawl the transcript cache.
    fn sweep(&mut self, now: Instant) -> usize {
        if self
            .last_sweep
            .is_some_and(|at| now.duration_since(at) < SEED_SWEEP_INTERVAL)
        {
            return 0;
        }
        self.last_sweep = Some(now);
        let resolved: Vec<(String, String)> = self
            .entries
            .keys()
            .filter(|(device, path)| {
                matches!(
                    crate::attachments::attachment_snapshot(device, path),
                    AttachmentSnapshot::Loaded(_)
                )
            })
            .cloned()
            .collect();
        let count = resolved.len();
        for (device, path) in resolved {
            self.remove(&device, &path);
        }
        let cutoff = now.checked_sub(SEED_TTL).unwrap_or(now);
        count + self.prune_before(cutoff)
    }
}

static SEEDED: OnceLock<Mutex<SeedCache>> = OnceLock::new();

fn seeded() -> &'static Mutex<SeedCache> {
    SEEDED.get_or_init(|| Mutex::new(SeedCache::default()))
}

/// The seed path an upload alias rewrites to on the host
/// (`{uploads}/{id8}-{name}`).
pub fn alias_path(upload_id: &str, name: &str) -> String {
    let id8: String = upload_id.chars().take(8).collect();
    format!("{id8}-{name}")
}

/// Seed the transcript's attachment cache with the bytes already in memory.
pub fn seed_attachment(device_id: &str, path: &str, name: &str, bytes: Arc<[u8]>) {
    seeded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(device_id, path, name, bytes);
}

/// Seed under an upload alias (`{uploads}/{id8}-{name}` rewrites on the host).
pub fn seed_attachment_alias(device_id: &str, upload_id: &str, name: &str, bytes: Arc<[u8]>) {
    seed_attachment(device_id, &alias_path(upload_id, name), name, bytes);
}

/// `(name, bytes)` for a seeded source, if this process staged it.
pub fn seeded_attachment(device_id: &str, path: &str) -> Option<(String, Arc<[u8]>)> {
    seeded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device_id, path)
}

/// Forget a seed once its send resolved (success or permanent failure) — the
/// transcript either has its own decoded copy or the bubble is gone.
pub fn forget_seeded(device_id: &str, path: &str) {
    seeded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device_id, path);
}

/// Retire seeds the transcript no longer needs: its thumbnail resolved, or
/// [`SEED_TTL`] elapsed. Called from the composer's per-frame poll (rate
/// limited internally); returns how many were dropped.
pub fn prune_seeded() -> usize {
    seeded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sweep(Instant::now())
}

/// Test-only construction without touching the filesystem.
#[cfg(test)]
pub(crate) fn staged_for_test(id: &str, bytes: &[u8]) -> StagedAttachment {
    StagedAttachment {
        id: id.to_string(),
        name: format!("{id}.png"),
        format: ImageFormat::Png,
        bytes: Arc::from(bytes.to_vec()),
    }
}

// ---------------------------------------------------------------------------
// Strip layout (attachment-ui.tsx AttachmentStrip)
// ---------------------------------------------------------------------------

pub const STRIP_THUMB: f32 = 56.0;
pub const STRIP_GAP: f32 = 8.0;
pub const STRIP_PAD_TOP: f32 = 12.0;
pub const STRIP_PAD_X: f32 = 16.0;
/// The staged-comment chip's height (matches the gpui `badges::BADGE_HEIGHT`).
pub const COMMENT_BADGE_HEIGHT: f32 = 24.0;

/// Height the wrap strip adds to the pill for `count` staged thumbnails at an
/// `inner_width` pill content width (0 when empty). Mirrors flex-wrap: as many
/// 56px thumbs per row as fit with 8px gaps inside the 16px side insets.
pub fn attachment_strip_height(count: usize, inner_width: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let usable = (inner_width - 2.0 * STRIP_PAD_X).max(STRIP_THUMB);
    let per_row = (((usable + STRIP_GAP) / (STRIP_THUMB + STRIP_GAP)).floor() as usize).max(1);
    let rows = count.div_ceil(per_row);
    STRIP_PAD_TOP + rows as f32 * STRIP_THUMB + (rows - 1) as f32 * STRIP_GAP
}

pub fn comment_strip_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    STRIP_PAD_TOP + COMMENT_BADGE_HEIGHT
}

// ---------------------------------------------------------------------------
// Upload (state.ts uploadAttachment)
// ---------------------------------------------------------------------------

fn with_target(mut params: serde_json::Value, target_device_id: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(map)) = (target_device_id, params.as_object_mut()) {
        map.insert("targetDeviceId".into(), target.into());
    }
    params
}

/// Per-call deadlines (desktop state.ts): a stalled-but-open relay link never
/// fails an RPC on its own, so every attachment call races a timer. The first
/// chunk gets 90s (a cold dial to a remote device), later chunks 30s; commit
/// 150s (it must outlast the engine's cross-device assemble); reads 20s.
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(90);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(150);
const READ_CHUNK_TIMEOUT: Duration = Duration::from_secs(20);

/// Race an RPC against `timeout` on tokio (the composer's async work runs on
/// the bridge's runtime).
pub async fn call_with_timeout(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    match tokio::time::timeout(timeout, client.call(method, params)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(format!("{method} timed out")),
    }
}

/// Chunks in flight at once. `seq` slots are idempotent engine-side.
const UPLOAD_CONCURRENCY: usize = 3;

/// Whole-attachment deadline. Per-chunk timeouts + retries bound each CALL,
/// but on a flapping link chunks that succeed on attempt 2-of-3 never trip the
/// 3-consecutive-failure abort — an upload could lawfully crawl for hours
/// reading "Sending…". Scaled with size, capped.
pub fn attachment_deadline(n_chunks: usize) -> Duration {
    Duration::from_secs((120 + 15 * n_chunks as u64).min(900))
}

/// The `(seq, b64 byte-range)` plan for a file's chunks. An empty file still
/// sends one empty chunk (the commit needs the uploadId staged).
pub fn chunk_ranges(b64_len: usize) -> Vec<(u64, std::ops::Range<usize>)> {
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

/// Chunked upload: base64 the bytes, `UploadChunk{uploadId,seq,data}` per
/// [`UPLOAD_CHUNK_B64_CHARS`] slice, a few chunks in flight at once, then
/// `UploadCommit{uploadId,fileName}` → the durable absolute path on the target
/// device. The caller mints `upload_id` — the queued-attachment flow derives
/// its `pending://` refs from the same identity before the bytes move.
pub async fn upload_attachment(
    client: &RpcClient,
    target_device_id: Option<&str>,
    upload_id: &str,
    attachment: &StagedAttachment,
    progress: Option<Arc<AtomicU64>>,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let ranges = chunk_ranges(b64.len());
    let deadline = attachment_deadline(ranges.len());

    let upload = async {
        use futures::TryStreamExt as _;
        futures::stream::iter(ranges.iter().cloned().map(Ok::<_, String>))
            .try_for_each_concurrent(UPLOAD_CONCURRENCY, |(seq, range)| {
                let progress = progress.clone();
                let b64 = &b64;
                async move {
                    let params = with_target(
                        serde_json::json!({
                            "uploadId": upload_id,
                            "seq": seq,
                            "data": &b64[range.clone()],
                        }),
                        target_device_id,
                    );
                    // The first WINDOW (not just seq 0) gets the cold-dial
                    // allowance — its chunks all start before the link is warm.
                    let timeout = if seq < UPLOAD_CONCURRENCY as u64 {
                        FIRST_CHUNK_TIMEOUT
                    } else {
                        CHUNK_TIMEOUT
                    };
                    // One transient blip must not abort the upload; `seq` slots
                    // are idempotent engine-side, so a blind re-send is safe.
                    let mut attempt = 0u32;
                    loop {
                        match call_with_timeout(
                            client,
                            methods::UPLOAD_CHUNK,
                            params.clone(),
                            timeout,
                        )
                        .await
                        {
                            Ok(_) => break,
                            Err(err) if attempt < 2 => {
                                attempt += 1;
                                tracing::warn!(error = %err, seq, attempt, "upload chunk retry");
                                // Stagger by seq so parallel chunks that failed
                                // together don't re-collide in lockstep.
                                tokio::time::sleep(Duration::from_millis(
                                    50 * (attempt as u64) * (seq + 1),
                                ))
                                .await;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    if let Some(progress) = &progress {
                        // b64 → binary bytes (padding rounds up by ≤2 bytes).
                        progress.fetch_add((range.len() * 3 / 4) as u64, Ordering::Relaxed);
                    }
                    Ok(())
                }
            })
            .await?;
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "fileName": attachment.name }),
            target_device_id,
        );
        let reply =
            call_with_timeout(client, methods::UPLOAD_COMMIT, params, COMMIT_TIMEOUT).await?;
        reply
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "upload commit returned no path".to_string())
    };
    futures::pin_mut!(upload);
    match tokio::time::timeout(deadline, upload).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "attachment upload exceeded {}s",
            deadline.as_secs()
        )),
    }
}

/// A transcript image read back from the owning device.
pub struct LoadedAttachmentImage {
    pub name: String,
    pub image: rune::Image,
}

/// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded, with
/// the same stuck-offset guard as hearth's `readAttachmentImage`).
pub async fn read_attachment_image(
    client: &RpcClient,
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
            client,
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
    let image = with_cache(|cache| cache.load(bytes));
    Some(LoadedAttachmentImage {
        name: if name.is_empty() {
            name_from_path(path)
        } else {
            name
        },
        image,
    })
}

/// True when `device_id`'s engine (per its registry device row) is at least
/// `min`. Unknown devices and unstamped versions are conservatively false —
/// feature gates fall back to the legacy path rather than speak a protocol the
/// peer may not understand.
pub fn device_version_at_least(
    devices: &[hearth_proto::Device],
    device_id: &str,
    min: (u64, u64, u64),
) -> bool {
    devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| d.version.as_deref())
        .and_then(version_triple)
        .is_some_and(|v| v >= min)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(ensure_extension("shot.png", ImageFormat::Png), "shot.png");
        assert_eq!(ensure_extension("image", ImageFormat::Png), "image.png");
        assert_eq!(
            ensure_extension("photo.j", ImageFormat::Jpeg),
            "photo.j.jpg"
        );
        assert_eq!(
            ensure_extension("archive.tar.gz", ImageFormat::Png),
            "archive.tar.gz"
        );
    }

    #[test]
    fn supported_formats_match_engine_jail() {
        for (ext, expect) in [
            ("png", Some(ImageFormat::Png)),
            ("JPG", Some(ImageFormat::Jpeg)),
            ("webp", Some(ImageFormat::Webp)),
            ("svg", Some(ImageFormat::Svg)),
            ("ico", None),
            ("txt", None),
        ] {
            assert_eq!(
                format_by_extension(Path::new(&format!("f.{ext}"))),
                expect,
                "ext {ext}"
            );
        }
        assert_eq!(
            ImageFormat::from_mime_type("image/jpeg"),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(ImageFormat::from_mime_type("text/plain"), None);
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
        assert_eq!(
            partial[1],
            (1, UPLOAD_CHUNK_B64_CHARS..UPLOAD_CHUNK_B64_CHARS + 7)
        );
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
    fn multi_path_staging_keeps_every_supported_file() {
        let paths = vec![
            PathBuf::from("/pick/one.png"),
            PathBuf::from("/pick/two.jpg"),
            PathBuf::from("/pick/notes.txt"),
            PathBuf::from("/pick/bad.png"),
        ];
        let out = stage_paths_with(&paths, |path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "bad.png" {
                Err("bad.png could not be read.".to_string())
            } else {
                let stem = path.file_stem().unwrap().to_string_lossy().to_string();
                Ok(staged_for_test(&stem, name.as_bytes()))
            }
        });
        assert_eq!(out.staged.len(), 2, "both supported files stage");
        assert_eq!(out.staged[0].id, "one");
        assert_eq!(out.staged[1].id, "two");
        assert_eq!(out.errors, vec!["bad.png could not be read.".to_string()]);
        // Unsupported paths never reach the reader.
        assert!(
            stage_paths_with(&[PathBuf::from("/pick/notes.txt")], |_| {
                panic!("unsupported extensions must be skipped")
            })
            .staged
            .is_empty()
        );
    }

    #[test]
    fn seed_cache_is_bounded_and_evicts_the_oldest() {
        let mut cache = SeedCache::default();
        let half = Arc::<[u8]>::from(vec![0u8; MAX_SEEDED_BYTES / 2]);
        assert!(cache.insert("dev", "a", "a.png", half.clone()));
        assert!(cache.insert("dev", "b", "b.png", half.clone()));
        assert_eq!(cache.entries.len(), 2);
        // A third half-budget file evicts the oldest until the budget holds.
        assert!(cache.insert("dev", "c", "c.png", half.clone()));
        assert!(cache.get("dev", "a").is_none(), "oldest seed evicted");
        assert!(cache.get("dev", "c").is_some());
        assert!(cache.retained_bytes <= MAX_SEEDED_BYTES);
        // A file larger than the whole budget is refused outright.
        let too_big = Arc::<[u8]>::from(vec![0u8; MAX_SEEDED_BYTES + 1]);
        assert!(!cache.insert("dev", "big", "big.png", too_big));
        assert!(cache.get("dev", "big").is_none());
    }

    #[test]
    fn seed_cache_entry_count_is_bounded() {
        let mut cache = SeedCache::default();
        let tiny = Arc::<[u8]>::from(vec![0u8; 8]);
        for ix in 0..(MAX_SEEDED_ENTRIES + 8) {
            let path = format!("{ix}.png");
            assert!(cache.insert("dev", &path, &path, tiny.clone()));
        }
        assert_eq!(cache.entries.len(), MAX_SEEDED_ENTRIES);
        assert!(cache.get("dev", "0.png").is_none(), "oldest evicted");
        let newest = format!("{}.png", MAX_SEEDED_ENTRIES + 7);
        assert!(cache.get("dev", &newest).is_some());
    }

    #[test]
    fn seed_cache_prunes_after_the_frame_budget() {
        let mut cache = SeedCache::default();
        assert!(cache.insert("dev", "a.png", "a.png", Arc::from(vec![0u8; 4])));
        let fresh_cutoff = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        assert_eq!(cache.prune_before(fresh_cutoff), 0);
        assert!(cache.get("dev", "a.png").is_some());
        assert_eq!(cache.prune_before(Instant::now()), 1);
        assert!(cache.get("dev", "a.png").is_none());
        assert_eq!(cache.retained_bytes, 0);
    }

    #[test]
    fn sweep_drops_seeds_once_the_transcript_thumbnail_resolved() {
        let mut cache = SeedCache::default();
        assert!(cache.insert(
            "sweep-dev",
            "resolved.png",
            "resolved.png",
            Arc::from(vec![0u8; 4])
        ));
        let attachment = staged_for_test("resolved", b"not-an-image");
        crate::attachments::store_loaded(
            "sweep-dev",
            "resolved.png",
            "resolved.png",
            Arc::new(thumbnail(&attachment)),
        );
        assert_eq!(cache.sweep(Instant::now()), 1);
        assert!(cache.get("sweep-dev", "resolved.png").is_none());
        // The sweep is rate limited: a second call inside the window is free.
        assert!(cache.insert(
            "sweep-dev",
            "later.png",
            "later.png",
            Arc::from(vec![0u8; 4])
        ));
        assert_eq!(cache.sweep(Instant::now()), 0);
    }

    #[test]
    fn forgetting_a_seed_releases_its_bytes() {
        let mut cache = SeedCache::default();
        assert!(cache.insert("dev", "a.png", "a.png", Arc::from(vec![0u8; 16])));
        assert_eq!(cache.retained_bytes, 16);
        assert!(cache.remove("dev", "a.png"));
        assert_eq!(cache.retained_bytes, 0);
        assert!(!cache.remove("dev", "a.png"));
    }

    #[test]
    fn bytes_arc_shares_the_staged_buffer() {
        let attachment = staged_for_test("shared", b"bytes");
        let shared = attachment.bytes_arc();
        assert!(std::ptr::eq(shared.as_ptr(), attachment.bytes().as_ptr()));
        assert_eq!(&*shared, b"bytes");
    }

    #[test]
    fn attachment_strip_wraps_by_width() {
        // One thumb is the top pad plus the thumb.
        assert_eq!(attachment_strip_height(0, 720.0), 0.0);
        assert_eq!(
            attachment_strip_height(1, 720.0),
            STRIP_PAD_TOP + STRIP_THUMB
        );
        // (720 - 32 + 8) / 64 = 10 per row.
        assert_eq!(
            attachment_strip_height(10, 720.0),
            STRIP_PAD_TOP + STRIP_THUMB
        );
        assert_eq!(
            attachment_strip_height(11, 720.0),
            STRIP_PAD_TOP + 2.0 * STRIP_THUMB + STRIP_GAP
        );
        // A narrow pill still fits one per row.
        assert_eq!(
            attachment_strip_height(2, 100.0),
            STRIP_PAD_TOP + 2.0 * STRIP_THUMB + STRIP_GAP
        );
        assert_eq!(comment_strip_height(0), 0.0);
        assert_eq!(
            comment_strip_height(2),
            STRIP_PAD_TOP + COMMENT_BADGE_HEIGHT
        );
    }
}
