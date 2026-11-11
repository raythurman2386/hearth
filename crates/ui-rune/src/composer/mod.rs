//! The composer: a Rune [`TextEdit`]-backed multiline prompt, the
//! compact↔expanded flip, the Send/Steer/Stop morph, optimistic send with
//! failure recovery, per-chat drafts, staged attachments, `@`/`/` completion,
//! and the question wizard that replaces the composer while a run awaits input.
//!
//! Pure decision logic lives in [`pure`] (flip, auto-grow math, button morph,
//! wizard reducer, pending-input detection) with its gpui-era unit tests; the
//! framework-only pieces are split into [`attachments`], [`mentions`],
//! [`slash`], [`send`] and [`render`].
//!
//! ## Wiring
//!
//! ```ignore
//! // App::poll (lead):
//! changed |= hearth_ui_rune::composer::poll(app);      // events + draft/wizard sync
//!
//! // wherever the composer column is drawn:
//! hearth_ui_rune::composer::render(app, &app.theme)
//! // after the root body is built (pickers + mention/slash popups):
//! hearth_ui_rune::composer::popups(app, &app.theme, &mut layer)
//! // once per frame, after the shell sees a send:
//! if let Some(sent) = hearth_ui_rune::composer::take_sent(app) { … }
//! // AppSpec::setup: give thumbnails their wake handle
//! hearth_ui_rune::composer::set_waker(waker);
//! // and the Edit menu's "Focus composer" wants:
//! hearth_ui_rune::composer::focus(app, tx);
//! ```
//!
//! Background work never touches [`crate::state::App`]: every task posts a
//! [`ComposerEvent`] on this state's channel and [`ComposerState::poll`] /
//! [`poll`] applies it on the UI thread.

pub mod attachments;
pub mod mentions;
pub mod pure;
pub mod render;
pub mod send;
pub mod slash;

pub use attachments::{PreviewImage, StagedAttachment};
pub use mentions::{FileMentionState, MentionToken};
pub use pure::{
    COMPACT_TOTAL_HEIGHT, COMPOSER_MAX_HEIGHT, COMPOSER_MAX_WIDTH, COMPOSER_MIN_HEIGHT,
    SendButtonMode, WizardStep,
};
pub use render::{lightbox_overlay, popups, render, render_question};
pub use slash::SlashState;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use hearth_doc::SessionMessageEntry;
use hearth_proto::{FileSearchMatch, HarnessId, SlashCommand, UserInputQuestion};

use rune::{TextEdit, Tx};

use crate::state::App;

use pure::{
    COMPACT_TOTAL_HEIGHT as COMPACT_HEIGHT, COMPOSER_MAX_WIDTH as MAX_WIDTH, FlipMorph,
    INPUT_LINE_HEIGHT, RESIZE_SETTLE_MS, ROUTE_SNAP_MS, Wizard, compact_input_capacity,
    composer_flip, composer_total_height, flip_morph_step, input_request_resolved,
    measured_flip_width, pending_input_request,
};

/// Placeholder copy the shared input shows in each mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder;

impl Placeholder {
    pub const COMPOSER: &str = "Do anything…";
    pub const WIZARD: &str = "Type your own answer, or pick an option above";
    pub const WIZARD_PICKED: &str =
        "Type your own answer, or leave this blank to use the selected option";
}

/// What a background task hands back to the UI thread.
pub enum ComposerEvent {
    MentionLoaded {
        request: u64,
        outcome: Result<Vec<FileSearchMatch>, String>,
    },
    CommandsLoaded {
        request: u64,
        harness: HarnessId,
        outcome: Result<Vec<SlashCommand>, String>,
    },
    WizardAdvance {
        request_id: String,
    },
    SendFinished(SendOutcome),
    Failure {
        key: Option<String>,
        message: String,
    },
    AnswerFinished {
        request_id: String,
        chat_id: String,
        error: Option<String>,
    },
    AnswerSettled {
        request_id: String,
    },
}

/// A finished send's restore payload (failure hand-back).
pub struct SendOutcome {
    pub chat_id: String,
    pub message_id: String,
    /// Where the prompt/files/comments go back (`""` = the new-chat canvas).
    pub restore_key: String,
    pub typed: String,
    pub is_new: bool,
    pub error: Option<String>,
    pub comments: Vec<crate::comments::DiffComment>,
    pub staged: Vec<StagedAttachment>,
}

/// A sent prompt the transcript should anchor (the shell takes it once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentEvent {
    pub chat_id: String,
    pub message_id: String,
}

/// The composer's whole state.
pub struct ComposerState {
    /// The live text model (Rune's built-in selection, undo, IME, pills).
    pub edit: TextEdit,
    /// Draft text per chat key (raw Markdown, so mention links survive
    /// navigation; `""` = new-chat canvas).
    pub drafts: HashMap<String, String>,
    /// Staged-but-unsent attachments per chat key.
    pub attachments: HashMap<String, Vec<StagedAttachment>>,
    /// Decoded thumbnails by attachment id (see [`attachments::thumbnail`]).
    pub thumbs: HashMap<String, rune::Image>,
    /// The staged attachment being viewed full-size.
    pub preview: Option<PreviewImage>,
    pub mention: FileMentionState,
    pub slash: SlashState,
    /// Advertised commands per harness (one `ListCommands` per harness).
    pub slash_cache: HashMap<HarnessId, Vec<SlashCommand>>,
    pub current_key: String,
    pub sending: bool,
    pub failure: Option<String>,
    /// The chat key `failure` belongs to (`None` = global).
    pub failure_key: Option<String>,
    pub wizard: Option<Wizard>,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    pub answered_requests: HashSet<String>,
    pub placeholder: String,

    // -- compact/expanded flip (hysteresis; see `composer_flip`) --
    expanded_mode: bool,
    /// Compact-mode input capacity, learned while compact.
    compact_capacity: f32,
    /// Last input/pill width seen in the current mode (resize detection).
    last_seen_width: f32,
    /// Stable outer composer width supplied by the layout.
    last_available_width: Option<f32>,
    /// Set while an interactive resize is in flight; collapse is deferred
    /// until widths have settled for `RESIZE_SETTLE_MS`.
    width_changed_at: Option<Instant>,
    /// In-flight compact↔expanded morph (reference timeline; Rune animates the
    /// pill's layout spring).
    flip_morph: Option<FlipMorph>,
    /// Pill height actually rendered last frame.
    last_rendered_height: f32,
    /// Measured input content height (wrapped lines × line height).
    field_height: f32,
    /// Monotonic clock anchor for the morph timeline.
    morph_clock: Instant,
    /// Set on every session/route change: flips committed before this instant
    /// SNAP instead of morphing.
    route_snap_until: Option<Instant>,

    sent: Option<SentEvent>,
    outcomes: Vec<ComposerEvent>,
    tx: Sender<ComposerEvent>,
    rx: Receiver<ComposerEvent>,
}

impl Default for ComposerState {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = Self {
            edit: TextEdit::new(""),
            drafts: HashMap::new(),
            attachments: HashMap::new(),
            thumbs: HashMap::new(),
            preview: None,
            mention: FileMentionState::default(),
            slash: SlashState::default(),
            slash_cache: HashMap::new(),
            current_key: String::new(),
            sending: false,
            failure: None,
            failure_key: None,
            wizard: None,
            answered_requests: HashSet::new(),
            placeholder: Placeholder::COMPOSER.to_string(),
            expanded_mode: false,
            compact_capacity: 0.0,
            last_seen_width: 0.0,
            last_available_width: None,
            width_changed_at: None,
            flip_morph: None,
            last_rendered_height: 0.0,
            field_height: INPUT_LINE_HEIGHT,
            morph_clock: Instant::now(),
            route_snap_until: None,
            sent: None,
            outcomes: Vec::new(),
            tx,
            rx,
        };
        // Dev knob: pre-stage attachments (drop/paste can't be synthesized on
        // a rig) — `HEARTH_ATTACH=/path/a.png[,/path/b.png]`.
        if let Ok(spec) = std::env::var("HEARTH_ATTACH") {
            let staged: Vec<StagedAttachment> = spec
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|path| {
                    match attachments::stage_file(std::path::Path::new(path.trim())) {
                        Ok(att) => {
                            state
                                .thumbs
                                .insert(att.id.clone(), attachments::thumbnail(&att));
                            Some(att)
                        }
                        Err(err) => {
                            tracing::warn!(%path, error = %err, "HEARTH_ATTACH stage failed");
                            None
                        }
                    }
                })
                .collect();
            if let Some(first) = staged.first()
                && std::env::var("HEARTH_ATTACH_PREVIEW").is_ok_and(|v| v == "1")
            {
                state.preview = Some(PreviewImage::of(first));
            }
            if !staged.is_empty() {
                state
                    .attachments
                    .entry(state.current_key.clone())
                    .or_default()
                    .extend(staged);
            }
        }
        state
    }
}

impl ComposerState {
    /// A sender for background tasks (one clone per spawned job).
    pub fn sender(&self) -> Sender<ComposerEvent> {
        self.tx.clone()
    }

    /// Whether a send is in flight.
    pub fn is_sending(&self) -> bool {
        self.sending
    }

    /// The composer column's max width (`max-w-3xl`).
    pub fn max_width(&self) -> f32 {
        MAX_WIDTH
    }

    /// Staged attachments for the chat the composer is showing.
    pub fn staged(&self) -> &[StagedAttachment] {
        self.attachments
            .get(&self.current_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Diff comments staged against the current chat.
    pub fn staged_comments<'a>(&self, app: &'a App) -> &'a [crate::comments::DiffComment] {
        app.diff_comments(&self.current_key)
    }

    /// The decoded thumbnail for a staged attachment, falling back to a decode
    /// when the cache entry was lost (drafts restored after a send failure).
    pub fn thumbnail(&self, attachment: &StagedAttachment) -> rune::Image {
        self.thumbs
            .get(&attachment.id)
            .cloned()
            .unwrap_or_else(|| attachments::thumbnail(attachment))
    }

    pub fn ensure_thumbnail(&mut self, attachment: &StagedAttachment) {
        self.thumbs
            .entry(attachment.id.clone())
            .or_insert_with(|| attachments::thumbnail(attachment));
    }

    /// Drop a deleted chat's per-chat composer state — staged attachments hold
    /// raw image bytes, and a deleted chat's stage could never be sent again.
    pub fn purge_chat(&mut self, chat_id: &str, app: &mut App) {
        self.attachments.remove(chat_id);
        self.drafts.remove(chat_id);
        app.purge_diff_comments(chat_id);
    }

    /// The layout's stable composer width (from the container's `on_layout`).
    pub fn note_available_width(&mut self, width: f32) -> bool {
        let width = width.clamp(0.0, MAX_WIDTH);
        let inner = (width - 2.0 * crate::theme::Theme::SPACE_LG).max(0.0);
        self.last_available_width = Some(width);
        self.compact_capacity = compact_input_capacity(inner);
        false
    }

    /// The pill's laid-out rect: tracks the width (interactive resizes) and the
    /// height actually rendered (the morph's start point).
    pub fn note_pill_layout(&mut self, rect: [f32; 4]) -> bool {
        let (width, height) = (rect[2], rect[3]);
        let mut changed = false;
        if self.last_seen_width > 0.0 && (width - self.last_seen_width).abs() > 0.5 {
            self.width_changed_at = Some(Instant::now());
        }
        self.last_seen_width = width;
        if height > 0.0 && (height - self.last_rendered_height).abs() > 0.25 {
            self.last_rendered_height = height;
            changed = true;
        }
        // The field's own `on_layout` may have run before the width was known
        // (handlers fire on change only, so it won't run again): re-evaluate
        // here, where the container width just landed.
        changed |= self.observe_flip();
        changed
    }

    /// The input's laid-out rect: drives auto-grow and the compact↔expanded
    /// flip. Runs from the field's `on_layout`, after layout, exactly where the
    /// gpui element fed its measurements to the same pure decision.
    pub fn note_field_layout(&mut self, rect: [f32; 4]) -> bool {
        self.field_height = rect[3];
        self.observe_flip()
    }

    /// The pill's last measured width (staged-strip wrap math).
    pub fn pill_width(&self) -> f32 {
        self.last_seen_width
    }

    /// The input's last measured content height (wrapped lines × 22.75).
    pub fn field_height(&self) -> f32 {
        self.field_height
    }

    /// Commit a mode change when the measured text no longer fits (or now fits
    /// comfortably) inside the compact capacity.
    fn observe_flip(&mut self) -> bool {
        if self.last_seen_width <= 0.0 {
            return false;
        }
        let capacity = compact_input_capacity(self.last_seen_width);
        let resizing = self
            .width_changed_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(RESIZE_SETTLE_MS));
        let text_width = measured_flip_width(self.field_height, capacity);
        let has_newline = self.edit.text.contains('\n');
        let next = composer_flip(
            self.expanded_mode,
            text_width,
            capacity,
            has_newline,
            resizing,
        );
        if next == self.expanded_mode {
            return false;
        }
        self.expanded_mode = next;
        let route_snap = self
            .route_snap_until
            .is_some_and(|until| Instant::now() < until);
        self.flip_morph = flip_morph_step(
            self.flip_morph,
            true,
            self.last_rendered_height,
            self.morph_now_ms(),
            false,
            route_snap,
        );
        true
    }

    pub fn expanded(&self) -> bool {
        self.expanded_mode
    }

    /// The pill's target height for the current mode, content and strips.
    pub fn target_height(&self, strip_height: f32, comment_strip: f32) -> f32 {
        self.target_height_for(self.expanded_mode, strip_height, comment_strip)
    }

    /// [`Self::target_height`] for the EFFECTIVE mode (new chats force the
    /// expanded layout before the first measurement commits it).
    pub fn target_height_for(&self, expanded: bool, strip_height: f32, comment_strip: f32) -> f32 {
        let base = if expanded {
            composer_total_height(self.field_height)
        } else {
            COMPACT_HEIGHT
        };
        base + strip_height + comment_strip
    }

    pub fn morph_now_ms(&self) -> f32 {
        self.morph_clock.elapsed().as_secs_f32() * 1000.0
    }

    pub fn morph(&self) -> Option<FlipMorph> {
        self.flip_morph.filter(|m| !m.done(self.morph_now_ms()))
    }

    pub fn set_sent(&mut self, chat_id: &str, message_id: &str) {
        self.sent = Some(SentEvent {
            chat_id: chat_id.to_string(),
            message_id: message_id.to_string(),
        });
    }

    /// The shell takes the last send once (to anchor the transcript prompt).
    pub fn take_sent(&mut self) -> Option<SentEvent> {
        self.sent.take()
    }

    pub fn take_outcomes(&mut self) -> Vec<ComposerEvent> {
        std::mem::take(&mut self.outcomes)
    }

    /// Drain background events. App-level outcomes (a finished send, a wizard
    /// advance) are queued for [`poll`]; the rest apply here.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                ComposerEvent::MentionLoaded { request, outcome } => {
                    mentions::apply_mention_results(&mut self.mention, request, outcome);
                }
                ComposerEvent::CommandsLoaded {
                    request,
                    harness,
                    outcome,
                } => {
                    if slash::apply_command_results(
                        &mut self.slash_cache,
                        &mut self.slash,
                        request,
                        harness,
                        outcome,
                    ) {
                        slash::refilter_slash_state(&mut self.slash, &self.slash_cache);
                    }
                }
                ComposerEvent::Failure { key, message } => {
                    self.failure = Some(message);
                    self.failure_key = key;
                }
                ComposerEvent::WizardAdvance { request_id } => {
                    if self
                        .wizard
                        .as_ref()
                        .is_some_and(|w| w.request_id == request_id)
                    {
                        self.outcomes
                            .push(ComposerEvent::WizardAdvance { request_id });
                    }
                }
                other @ (ComposerEvent::SendFinished(_)
                | ComposerEvent::AnswerFinished { .. }
                | ComposerEvent::AnswerSettled { .. }) => self.outcomes.push(other),
            }
        }
        // A finished morph stops requesting frames.
        if let Some(morph) = self.flip_morph
            && morph.done(self.morph_now_ms())
        {
            self.flip_morph = None;
            changed = true;
        }
        changed
    }

    /// Move the draft to `key` (the selected chat, `""` for the canvas).
    fn swap_draft(&mut self, key: &str) -> bool {
        if self.current_key == key {
            return false;
        }
        let old_text = mentions::prompt_text(&self.edit);
        if old_text.is_empty() {
            self.drafts.remove(&self.current_key);
        } else {
            self.drafts.insert(self.current_key.clone(), old_text);
        }
        let draft = self.drafts.get(key).cloned().unwrap_or_default();
        self.current_key = key.to_string();
        // `failure` deliberately survives navigation: chat-scoped failures
        // render only under their own chat, so switching away and back must not
        // erase the one visible trace of a failed send.
        self.wizard = None;
        // Attachments stay stashed under their chat key (the map swap IS the
        // navigation); only the transient chrome resets.
        self.preview = None;
        mentions::dismiss_mention_state(self);
        slash::dismiss_slash_state(&mut self.slash);
        // Route changes snap: a mode difference between the old and new
        // session's composer must not glide across navigation.
        self.flip_morph = None;
        self.last_rendered_height = 0.0;
        self.route_snap_until = Some(Instant::now() + Duration::from_millis(ROUTE_SNAP_MS));
        mentions::set_draft(&mut self.edit, &draft);
        true
    }
}

/// The per-frame composer hook: swap drafts on navigation, keep the question
/// panel's lifecycle, drain background events, and apply the App-level
/// outcomes (send hand-back, wizard advance). Returns true when visible state
/// changed.
pub fn poll(app: &mut App) -> bool {
    let mut changed = sync(app);
    // Feed the measured pill width into the pickers' compact-traits
    // presentation (one frame stale, threshold-crossing only).
    let width = app.composer.pill_width();
    if width > 0.0 {
        crate::pickers::set_composer_width(app, (width - 32.0).max(0.0));
    }
    changed |= app.composer.poll();
    for event in app.composer.take_outcomes() {
        changed = true;
        match event {
            ComposerEvent::SendFinished(outcome) => send::apply_send_outcome(app, outcome),
            ComposerEvent::WizardAdvance { request_id } => {
                if app
                    .composer
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id)
                {
                    send::wizard_advance(app);
                }
            }
            ComposerEvent::AnswerFinished {
                request_id,
                chat_id,
                error,
            } => {
                if let Some(message) = error {
                    app.composer.failure = Some(message);
                    app.composer.failure_key = Some(chat_id);
                    // The answer never left this device — put the panel back.
                    app.composer.answered_requests.remove(&request_id);
                }
            }
            ComposerEvent::AnswerSettled { request_id } => {
                // Safety net against a dead-looking session: the command
                // queued, but the host may still REJECT it. If the very same
                // request is still the live pending input, un-hide the panel.
                let still_pending = pending_input_request(&app.transcript)
                    .is_some_and(|(pending_id, _)| pending_id == request_id);
                if still_pending && app.composer.answered_requests.remove(&request_id) {
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Draft swap + question-panel lifecycle (the gpui `on_state_changed`).
fn sync(app: &mut App) -> bool {
    let key = app.composer_key();
    let mut changed = app.composer.swap_draft(&key);
    let pending = pending_input_request(&app.transcript);
    match pending {
        Some((request_id, questions)) if !app.composer.answered_requests.contains(&request_id) => {
            let same = app
                .composer
                .wizard
                .as_ref()
                .is_some_and(|w| w.request_id == request_id);
            if !same {
                mentions::dismiss_mention_state(&mut app.composer);
                slash::dismiss_slash_state(&mut app.composer.slash);
                app.composer.wizard = Some(Wizard::new(request_id, questions));
                // The shared input becomes the panel's free-text override.
                app.composer.placeholder = Placeholder::WIZARD.to_string();
                changed = true;
            }
        }
        _ => {
            if let Some(wizard) = app.composer.wizard.as_ref() {
                // LATCH (original composer.tsx `inputLatch`): a transient
                // fold/sync blip — or a steer appended behind the streaming
                // entry — must not unmount the panel and lose the user's picks.
                // Release only on explicit resolution (here or on another
                // device) or when a NON-EMPTY transcript shows the question
                // superseded (a newer assistant entry took over). Never on run
                // death: the question stays answerable until answered.
                let request_id = wizard.request_id.clone();
                let released = input_request_resolved(&app.transcript, &request_id)
                    || (!app.transcript.is_empty()
                        && !app.composer.answered_requests.contains(&request_id));
                if released {
                    app.composer.wizard = None;
                    app.composer.placeholder = Placeholder::COMPOSER.to_string();
                    changed = true;
                }
            }
        }
    }
    changed
}

/// The shell's once-per-frame take of a send (anchors the transcript prompt).
pub fn take_sent(app: &mut App) -> Option<SentEvent> {
    app.composer.take_sent()
}

/// Focus the prompt input (the shell's focus fallback, `⌘L`-style actions and
/// the lightbox's hand-back all route here).
pub fn focus(app: &mut App, tx: &mut Tx) {
    app.composer.edit.focused = true;
    // A keyed focus also moves keyboard focus when the field itself is being
    // mounted this frame; the model focus above wins when it already exists.
    tx.focus("composer/input", false);
}

/// Stage files dropped on the conversation column / picked with the paperclip.
pub fn add_paths(app: &mut App, paths: Vec<PathBuf>) {
    let mut staged = Vec::new();
    for path in &paths {
        if attachments::format_by_extension(path).is_none() {
            continue;
        }
        match attachments::stage_file(path) {
            Ok(att) => {
                app.composer.ensure_thumbnail(&att);
                staged.push(att);
            }
            Err(message) => {
                app.composer.failure = Some(message);
                app.composer.failure_key = Some(app.composer.current_key.clone());
            }
        }
    }
    add_staged(app, staged);
}

/// Stage already-read attachments (clipboard images, restored sends).
pub fn add_staged(app: &mut App, staged: Vec<StagedAttachment>) {
    if staged.is_empty() {
        return;
    }
    for att in &staged {
        app.composer.ensure_thumbnail(att);
    }
    let key = app.composer.current_key.clone();
    app.composer
        .attachments
        .entry(key)
        .or_default()
        .extend(staged);
}

/// Remove a staged attachment from the current chat's strip.
pub fn remove_attachment(app: &mut App, id: &str) {
    let key = app.composer.current_key.clone();
    if let Some(list) = app.composer.attachments.get_mut(&key) {
        list.retain(|a| a.id != id);
        if list.is_empty() {
            app.composer.attachments.remove(&key);
        }
    }
    app.composer.thumbs.remove(id);
}

/// The field's `on_input_change` hook: refresh `/` and `@` completion.
pub fn on_edit(app: &mut App) {
    if app.composer.wizard.is_some() {
        if app.composer.mention.token.is_some() {
            mentions::dismiss_mention_state(&mut app.composer);
        }
        if app.composer.slash.token.is_some() {
            slash::dismiss_slash_state(&mut app.composer.slash);
        }
        return;
    }
    let text = app.composer.edit.text.clone();
    let cursor = app.composer.edit.range().1;
    slash::update_slash(app, &text, cursor);
    mentions::update_mention(app, &text, cursor);
}

/// The field's `on_input_key`: completion navigation/accept, and the wizard's
/// number/Enter/Escape routing while the free-text override is empty.
pub fn on_input_key(app: &mut App, ev: &rune::KeyEvent) -> bool {
    let has_completion = app.composer.slash.token.is_some() || app.composer.mention.token.is_some();
    if has_completion {
        let slash_open = app.composer.slash.token.is_some();
        match ev.key {
            rune::Key::Up => {
                if slash_open {
                    slash::move_slash(app, -1);
                } else {
                    mentions::move_mention(app, -1);
                }
                return true;
            }
            rune::Key::Down => {
                if slash_open {
                    slash::move_slash(app, 1);
                } else {
                    mentions::move_mention(app, 1);
                }
                return true;
            }
            rune::Key::Enter | rune::Key::Tab => {
                // Accept only a highlighted row: with an empty result list the
                // key belongs to the composer (Enter submits the prompt).
                if slash_open {
                    if app.composer.slash.active.is_none() {
                        return false;
                    }
                    slash::accept_slash(app);
                } else {
                    if app.composer.mention.active.is_none() {
                        return false;
                    }
                    mentions::accept_mention(app);
                }
                return true;
            }
            rune::Key::Escape => {
                if slash_open {
                    slash::dismiss_slash(app);
                } else {
                    mentions::dismiss_mention(app);
                }
                return true;
            }
            _ => {}
        }
    }
    if app.composer.wizard.is_some() {
        let empty = app.composer.edit.text.is_empty();
        if let rune::Key::Text(text) = &ev.key
            && empty
            && !ev.mods.cmd
            && !ev.mods.ctrl
            && !ev.mods.alt
            && let Some(digit) = text.chars().next().and_then(|c| c.to_digit(10))
            && (1..=9).contains(&digit)
        {
            send::wizard_select(app, digit as usize - 1);
            return true;
        }
        if empty && matches!(ev.key, rune::Key::Escape) {
            send::wizard_back(app);
            return true;
        }
    }
    false
}

/// The window-level fallback for the wizard panel (when the free-text input
/// does not hold focus): digits select, Enter advances, Escape pages back.
pub fn on_wizard_key(app: &mut App, ev: &rune::KeyEvent) -> bool {
    if app.composer.wizard.is_none() {
        return false;
    }
    let empty = app.composer.edit.text.is_empty();
    match &ev.key {
        rune::Key::Text(text) if empty => {
            if let Some(digit) = text.chars().next().and_then(|c| c.to_digit(10))
                && (1..=9).contains(&digit)
            {
                send::wizard_select(app, digit as usize - 1);
                return true;
            }
            false
        }
        rune::Key::Enter => {
            send::wizard_advance(app);
            true
        }
        rune::Key::Escape if empty => {
            send::wizard_back(app);
            true
        }
        _ => false,
    }
}

/// Give the composer's image cache the app's wake handle.
pub fn set_waker(waker: rune::runtime::Waker) {
    attachments::set_waker(waker);
}

/// The question panel's input questions (the shell may ask before rendering).
pub fn pending_questions(
    transcript: &[SessionMessageEntry],
) -> Option<(String, Vec<UserInputQuestion>)> {
    pending_input_request(transcript)
}
