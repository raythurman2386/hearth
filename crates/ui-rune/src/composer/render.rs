//! Rendering for the composer: the pill/collapsed and expanded layouts, the
//! actions cluster (pickers + paperclip + Send/Steer/Stop), the footer toolbar,
//! notices, the question panel, the staged-attachment strip and the lightbox.
//!
//! Layout numbers are the gpui composer's constants (see [`super::pure`]);
//! colors come from [`crate::theme::Theme`].
//!
//! The view takes `&App`, so measurements are written back by the elements'
//! `on_layout` handlers ([`super::ComposerState::note_pill_layout`] and
//! [`super::ComposerState::note_field_layout`]) between frames.

use rune::{Color, El, PillStyle, Side, TextStyle, Tx, col, el, icon, row, text, text_input};
use rune_kit::{Button, ButtonVariant, ControlSize, PopupItem, PopupList};

use crate::icons;
use crate::pickers;
use crate::state::App;
use crate::theme::Theme;

use super::attachments::{self, PreviewImage, StagedAttachment};
use super::mentions;
use super::pure::{
    self, ACTION_PRIMARY_GAP, ACTION_UTILITY_GAP, COMPACT_TOTAL_HEIGHT, COMPOSER_MAX_WIDTH,
    PILL_BORDER_V,
};
use super::send;
use super::slash;
use super::{ComposerState, Placeholder, add_paths, add_staged, on_edit, on_input_key};

/// The input element's key (the completion popups anchor to it).
pub const INPUT_KEY: &str = "composer/input";

fn style(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

fn body_style() -> TextStyle {
    TextStyle::new(pure::INPUT_TEXT_SIZE, 400.0).lh(pure::INPUT_LINE_HEIGHT)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The composer column: notices, the pill (or the question panel), and the
/// pickers' footer toolbar. The shell places this under the transcript.
///
/// Pair it with [`popups`] in the same frame so picker menus, `@`/`/`
/// completion and the attachment lightbox mount above the window.
pub fn render(app: &App, theme: &Theme) -> El {
    let composer = &app.composer;
    let mut container = col()
        .key("composer-root")
        .w_full()
        .max_w(COMPOSER_MAX_WIDTH)
        .gap(Theme::SPACE_SM)
        .px(Theme::SPACE_LG)
        .pb(Theme::SPACE_LG);

    // A chat-scoped failure renders only under its own chat; a global failure
    // (no key) renders everywhere.
    let failure = composer.failure.clone().filter(|_| {
        composer
            .failure_key
            .as_ref()
            .is_none_or(|key| *key == composer.current_key)
    });
    if let Some(message) = failure {
        container = container.child(failure_notice(theme, &message));
    }

    // Composer honesty: when the target's delivery path is degraded, say UP
    // FRONT that a send will queue instead of letting the button imply instant
    // delivery.
    if let Some((notice, offline)) = queue_notice(app) {
        container = container.child(queue_notice_line(theme, &notice, offline));
    }

    if let Some(percent) = app.upload_progress_percent() {
        container = container.child(
            row().key("composer-upload-progress").mx(8.0).mt(6.0).child(
                text(format!("Uploading… {percent}%"))
                    .font(style(11.0, 400.0).lh(14.0))
                    .color(theme.text_faint),
            ),
        );
    }

    // Turn-boundary steering notice: for agents without mid-turn injection, a
    // "steer" is queued and applies when the current turn finishes.
    if send::button_mode(app) == pure::SendButtonMode::Steer
        && app.pickers.resolved_steering_mode(app) == Some(hearth_proto::SteeringMode::TurnBoundary)
    {
        container = container.child(
            el().mt(6.0).px(12.0).child(
                text(
                    "This agent can't be steered mid-turn — your message will be queued and sent when the current turn finishes.",
                )
                .font(style(11.0, 400.0).lh(15.0))
                .color(theme.text_muted.alpha(0.8)),
            ),
        );
    }

    if composer.wizard.is_some() {
        return container.child(render_question(app, theme));
    }

    // New chats always use the expanded layout: the repo/branch pickers need
    // the full-width actions row.
    let new_chat = app.selected_chat.is_none();
    let expanded = composer.expanded() || new_chat;

    let staged = composer.staged();
    let strip_width_hint = if composer.pill_width() > 0.0 {
        composer.pill_width()
    } else {
        720.0
    };
    let strip_h = attachments::attachment_strip_height(staged.len(), strip_width_hint);
    let comment_count = composer.staged_comments(app).len();
    let comment_h = attachments::comment_strip_height(comment_count);
    let target_height = composer.target_height_for(expanded, strip_h, comment_h);
    let morphing = composer.morph().is_some();

    // Staged-thumbnail strip and comments chip sit above the input inside the
    // pill in BOTH modes.
    let strips = col()
        .w_full()
        .children(render_comments_chip(theme, comment_count))
        .children(render_attachment_strip(app, theme));

    let pill = if expanded {
        expanded_pill(app, theme, target_height, morphing, strips)
    } else {
        compact_pill(app, theme, target_height, morphing, strips)
    }
    .on_layout(|app: &mut App, rect: [f32; 4]| {
        app.composer.note_pill_layout(rect);
    })
    .on_drop_files(
        |app: &mut App, files: &[std::path::PathBuf], _tx: &mut Tx| {
            add_paths(app, files.to_vec());
        },
    )
    .accepts_extensions(&[
        "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff",
    ]);

    container = container.child(pill);

    // Branch/worktree toolbar under the pill (read-only labels once the
    // session exists). Git spaces only.
    if let Some(footer) = pickers::render_footer(&app.pickers, app, theme) {
        container = container.child(footer);
    }

    // Centered column (`mx-auto w-full max-w-3xl`).
    row().w_full().justify_center().child(container)
}

/// The question panel, rendered in place of the composer while a run awaits
/// input: the same floating-pill chrome, an uppercase header + "1/3" counter,
/// option rows with number kbd chips, a free-text override over a hairline, and
/// Back / Next-Submit.
pub fn render_question(app: &App, theme: &Theme) -> El {
    let Some(wizard) = app.composer.wizard.clone() else {
        return el();
    };
    let counter = wizard.counter();
    let Some(question) = wizard.current().cloned() else {
        return el();
    };
    let page = wizard.page;
    let last = page + 1 >= wizard.questions.len();
    let typed_empty = app.composer.edit.text.is_empty();
    let can_advance = wizard.page_has_pick() || !typed_empty;

    let mut options = col().mt(12.0).gap(4.0);
    for (ix, label) in question.options.iter().enumerate() {
        // Selection reads on the row only while no typed override exists
        // (typed answers win — hearth question-panel.tsx `isSel`).
        let picked = wizard.is_picked(ix) && typed_empty;
        let mut option = row()
            .key(format!("wizard-option-{ix}"))
            .w_full()
            .gap(12.0)
            .px(14.0)
            .py(10.0)
            .radius(12.0)
            .border(
                1.0,
                if picked {
                    theme.ink(0.16)
                } else {
                    Color::TRANSPARENT
                },
            )
            .fill(if picked {
                theme.ink(0.09)
            } else {
                theme.ink(0.025)
            })
            .hover(theme.ink(0.06))
            .cursor(rune::Cursor::Pointer)
            .center_y()
            .on_press(move |app: &mut App| send::wizard_select(app, ix))
            .child(
                text(label.clone())
                    .font(style(13.5, 500.0))
                    .color(if picked {
                        theme.text
                    } else {
                        theme.text.alpha(0.9)
                    })
                    .grow(1.0)
                    .min_w(0.0)
                    .wrap(),
            );
        if ix < 9 {
            option =
                option.child(
                    el().size(22.0, 22.0)
                        .radius(6.0)
                        .center()
                        .fill(if picked {
                            theme.ink(0.16)
                        } else {
                            theme.ink(0.05)
                        })
                        .child(text(format!("{}", ix + 1)).font(style(11.0, 400.0)).color(
                            if picked {
                                theme.text
                            } else {
                                theme.text_muted.alpha(0.6)
                            },
                        )),
                );
        }
        options = options.child(option);
    }

    let header = row()
        .gap(10.0)
        .center_y()
        .child(
            text(question.header.to_uppercase())
                .font(style(10.5, 500.0).tracking(0.8))
                .color(theme.text_muted.alpha(0.6)),
        )
        .children((wizard.questions.len() > 1).then(|| {
            el().h(20.0)
                .px(6.0)
                .radius(6.0)
                .center()
                .fill(theme.ink(0.06))
                .child(
                    text(counter)
                        .font(style(10.0, 500.0))
                        .color(theme.text_muted.alpha(0.6)),
                )
        }));

    let mut body = col().w_full().px(16.0).pt(16.0).child(header).child(
        text(question.question.clone())
            .font(style(15.0, 500.0).lh(20.0))
            .color(theme.text)
            .mt(6.0)
            .wrap(),
    );
    if question.multi_select {
        body = body.child(
            text("Select one or more options.")
                .font(style(12.0, 400.0))
                .color(theme.text_muted.alpha(0.65))
                .mt(4.0),
        );
    }
    body = body.child(options).child(
        el().mt(12.0)
            .pt(12.0)
            .pb(4.0)
            .px(4.0)
            .border_sides([1.0, 0.0, 0.0, 0.0], theme.hairline(0.06))
            .child(text_input_element(app, theme)),
    );

    let footer = row()
        .justify_between()
        .center_y()
        .px(16.0)
        .pb(16.0)
        .pt(4.0)
        .child(if page > 0 {
            Button::new("Back")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|app: &mut App| send::wizard_back(app))
                .build(&theme.kit)
        } else {
            el()
        })
        .child(
            Button::new(if last { "Submit" } else { "Next" })
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Small)
                .disabled(!can_advance)
                .on_press(|app: &mut App| send::wizard_advance(app))
                .build(&theme.kit),
        );

    col()
        .key("question-panel")
        .w_full()
        .radius(26.0)
        .border(1.0, theme.border)
        .fill(theme.input_glass_bg())
        .shadow(1.0, 24.0, theme.ink(0.10))
        .on_key(|app: &mut App, ev: &rune::KeyEvent, _tx: &mut Tx| super::on_wizard_key(app, ev))
        .child(body)
        .child(footer)
}

/// Picker menus, `@`/`/` completion and the attachment lightbox, mounted into
/// the window layer (`rune_kit::root(body, layer)`).
pub fn popups(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    pickers::popovers(&app.pickers, app, theme, layer);
    mention_popup(app, theme, layer);
    slash_popup(app, theme, layer);
    if let Some(overlay) = lightbox_overlay(app, theme) {
        layer.push(overlay);
    }
}

/// The full-size staged-attachment preview (the shell may also mount this
/// itself; [`popups`] does).
pub fn lightbox_overlay(app: &App, theme: &Theme) -> Option<El> {
    let preview = app.composer.preview.clone()?;
    Some(attachments::lightbox(theme, &preview, |app: &mut App| {
        app.composer.preview = None;
        app.composer.edit.focused = true;
    }))
}

// ---------------------------------------------------------------------------
// Pill layouts
// ---------------------------------------------------------------------------

fn pill_shell(theme: &Theme) -> El {
    // hearth composer.tsx: `rounded-[26px] border border-white/[0.08]
    // bg-white/[0.03] shadow-xl` — a floating pill with a hairline over a faint
    // wash, never a solid grey box. No shadow on glass.
    col()
        .radius(26.0)
        .fill(theme.input_glass_bg())
        .border(1.0, theme.border)
        .shadow(1.0, 24.0, theme.ink(0.10))
        .clip_rounded(26.0)
}

fn expanded_pill(app: &App, theme: &Theme, height: f32, morphing: bool, strips: El) -> El {
    let composer = &app.composer;
    let text_box = (pure::composer_total_height(composer.field_height())
        - PILL_BORDER_V
        - pure::ACTIONS_ROW_HEIGHT)
        .max(0.0);
    // The whole control cluster stays at full alpha — chips, attach and send
    // are all (near-)stationary on the bottom anchor.
    let cluster_dy = pure::morph_cluster_dy(1.0);
    let send_button = send_button(app, theme);

    pill_shell(theme)
        .h(height)
        .motion(if morphing {
            crate::motion::COLLAPSE
        } else {
            rune_motion::Spring::new(0.001, 0.0)
        })
        .justify_start()
        .child(strips)
        .child(
            col()
                .w_full()
                .h(text_box)
                .px(16.0)
                .pt(pure::morph_text_pad(1.0))
                .pb(4.0)
                .child(text_input_element(app, theme)),
        )
        .child(
            row()
                .pin(Some(0.0), None, Some(0.0), Some(-cluster_dy))
                .h(pure::ACTIONS_ROW_HEIGHT)
                .gap(ACTION_PRIMARY_GAP)
                .pl(12.0)
                .pr(pure::morph_cluster_inset(true, 1.0))
                .pt(4.0)
                .pb(10.0)
                .center_y()
                .child(
                    row()
                        .grow(1.0)
                        .min_w(0.0)
                        .justify_end()
                        .center_y()
                        .gap(ACTION_UTILITY_GAP)
                        // `render_cluster` carries the footer row too; inside the
                        // pill only its h-8 chip row belongs. The footer is drawn
                        // under the pill below.
                        .child(
                            el().grow(1.0)
                                .min_w(0.0)
                                .h(32.0)
                                .clip_soft(0.0)
                                .child(pickers::render_cluster(&app.pickers, app, theme)),
                        )
                        .child(attach_button(theme)),
                )
                .child(send_button),
        )
}

fn compact_pill(app: &App, theme: &Theme, height: f32, morphing: bool, strips: El) -> El {
    let send_button = send_button(app, theme);
    // A narrow pill can't fit the picker cluster beside the text; keep the
    // field usable by collapsing the cluster to attach + send.
    let narrow = app.composer.pill_width() < 360.0;
    pill_shell(theme)
        .h(height)
        .motion(if morphing {
            crate::motion::COLLAPSE
        } else {
            rune_motion::Spring::new(0.001, 0.0)
        })
        .justify_end()
        .child(strips)
        .child(
            row()
                .h(COMPACT_TOTAL_HEIGHT - PILL_BORDER_V)
                .center_y()
                .child(
                    el().grow(1.0)
                        .min_w(if narrow { 80.0 } else { 0.0 })
                        .pl(16.0)
                        .pr(8.0)
                        .child(text_input_element(app, theme)),
                )
                .child(
                    row()
                        .center_y()
                        .gap(ACTION_PRIMARY_GAP)
                        .pl(4.0)
                        .pr(pure::morph_cluster_inset(false, 1.0))
                        .child(
                            row()
                                .center_y()
                                .gap(ACTION_UTILITY_GAP)
                                .children(if narrow {
                                    None
                                } else {
                                    Some(
                                        el().h(32.0).clip_soft(0.0).child(pickers::render_cluster(
                                            &app.pickers,
                                            app,
                                            theme,
                                        )),
                                    )
                                })
                                .child(attach_button(theme)),
                        )
                        .child(send_button),
                ),
        )
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn text_input_element(app: &App, theme: &Theme) -> El {
    let bg = theme.ink(0.06);
    let fg = theme.text;
    let field = text_input(&app.composer.edit, body_style())
        .key(INPUT_KEY)
        .placeholder(
            app.composer.placeholder.clone(),
            theme.text_muted.alpha(0.65),
        )
        .caret(theme.caret, theme.selection)
        .on_edit(|app: &mut App| &mut app.composer.edit)
        .on_submit(|app: &mut App, _tx: &mut Tx| send::submit(app))
        .on_input_key(|app: &mut App, ev: &rune::KeyEvent, _tx: &mut Tx| on_input_key(app, ev))
        .on_input_change(|app: &mut App, _tx: &mut Tx| on_edit(app))
        .on_input_paste(
            |app: &mut App, content: &rune::ClipboardContent, _tx: &mut Tx| {
                if let Some(image) = content.image() {
                    let staged = attachments::stage_clipboard_image(image.png());
                    add_staged(app, vec![staged]);
                    return true;
                }
                // A file manager's "Copy" puts paths on the clipboard; stage
                // the images among them instead of pasting their paths as text.
                let files: Vec<std::path::PathBuf> = content
                    .files()
                    .iter()
                    .filter(|path| attachments::format_by_extension(path).is_some())
                    .cloned()
                    .collect();
                if files.is_empty() {
                    return false;
                }
                add_paths(app, files);
                true
            },
        )
        .on_layout(|app: &mut App, rect: [f32; 4]| {
            app.composer.note_field_layout(rect);
        })
        .pill_style(move |id: &str| {
            if mentions::is_mention_pill(id) {
                PillStyle::new().bg(bg).fg(fg).icon(&icons::DOCUMENT)
            } else {
                PillStyle::new().bg(bg).fg(fg)
            }
        })
        .pill_icon(|id: &str| {
            if mentions::is_mention_pill(id) {
                Some(&icons::DOCUMENT)
            } else {
                None
            }
        });
    // 76–260 box: ~10 wrapped lines at 22.75 each; beyond that the field
    // scrolls internally (the gpui input's `scroll_top` behaviour).
    rune::auto_grow(field, 1, 10)
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn send_button(app: &App, theme: &Theme) -> El {
    match send::button_mode(app) {
        pure::SendButtonMode::Stop => el()
            .key("composer-stop")
            .size(28.0, 28.0)
            .radius(14.0)
            .fill(theme.text)
            .center()
            .cursor(rune::Cursor::Pointer)
            .hover(theme.text.alpha(0.85))
            .on_press(|app: &mut App| send::interrupt(app))
            .child(el().size(11.0, 11.0).radius(3.0).fill(theme.bg)),
        pure::SendButtonMode::Send | pure::SendButtonMode::Steer => {
            // Dimmed and inert while no project is picked or no agent is
            // runnable (`send_blocked` also gates submit, so Enter is a no-op).
            let blocked = send::send_blocked(app);
            let button = el()
                .key("composer-send")
                .size(28.0, 28.0)
                .radius(14.0)
                .fill(theme.text)
                .center()
                .child(icon(&icons::ARROW_UP, 14.0).color(theme.bg));
            if blocked {
                button.opacity(0.35)
            } else {
                button
                    .cursor(rune::Cursor::Pointer)
                    .hover(theme.text.alpha(0.85))
                    .on_press(|app: &mut App| send::submit(app))
            }
        }
    }
}

fn attach_button(theme: &Theme) -> El {
    // Opens the native image picker; paste/drop also feed the same strip. The
    // parent cluster owns the spacing (adding a second margin here made the
    // picker→attachment gap twice as wide as attachment→send).
    let options = rune::FileDialogOptions {
        title: "Attach".into(),
        directory: None,
        file_name: None,
        filters: Vec::new(),
    }
    .filter(
        "Images",
        &[
            "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff",
        ],
    );
    el().key("composer-attach")
        .size(28.0, 28.0)
        .radius(14.0)
        .center()
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(move |_app: &mut App, tx: &mut Tx| {
            let options = options.clone();
            tx.open_file(options, |app: &mut App, result, _tx| match result {
                Ok(Some(path)) => add_paths(app, vec![path]),
                Ok(None) => {}
                Err(err) => {
                    app.composer.failure = Some(format!("Couldn't open the file picker: {err}"));
                    app.composer.failure_key = Some(app.composer.current_key.clone());
                }
            });
        })
        // The source path's painted bounds are centered at x=11 inside a 24px
        // viewbox; correct that optical offset while keeping the 28px hit
        // target geometrically centered.
        .child(
            icon(&icons::PAPERCLIP, 16.0)
                .ml(1.0)
                .color(theme.text_muted),
        )
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

fn failure_notice(theme: &Theme, message: &str) -> El {
    // hearth composer.tsx `Notice` (matches the transcript ErrorChip palette):
    // a subtle tinted wash, not a bare red stroke. Amber for the offline-ish
    // case (engine not connected), red for send/run failures. Click dismisses.
    let offline = message == "Engine not connected";
    let (border_c, wash, text_c) = if offline {
        (
            theme.warning.alpha(0.16),
            theme.warning.alpha(0.05),
            theme.warning_muted.alpha(0.9),
        )
    } else {
        (
            theme.danger.alpha(0.16),
            theme.danger.alpha(0.05),
            theme.danger_muted.alpha(0.9),
        )
    };
    row()
        .key("composer-failure")
        .mx(4.0)
        .mt(6.0)
        .gap(8.0)
        .items_start()
        .radius(12.0)
        .border(1.0, border_c)
        .fill(wash)
        .px(12.0)
        .py(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(|app: &mut App| {
            app.composer.failure = None;
            app.composer.failure_key = None;
        })
        .child(icon(&icons::DANGER_TRIANGLE, 14.0).mt(2.0).color(text_c))
        .child(
            text(message.to_string())
                .font(style(12.0, 400.0).lh(16.0))
                .color(text_c)
                .min_w(0.0)
                .wrap(),
        )
}

fn queue_notice(app: &App) -> Option<(String, bool)> {
    use hearth_proto::ConnectivityState as S;
    let degraded = match app.selected_chat.as_deref() {
        Some(id) => app.chat_delivery_degraded(id),
        None => {
            // New-chat canvas: judge by the picked target device.
            let remote_target = app
                .effective_device_id()
                .is_some_and(|id| app.local_device_id.as_deref() != Some(id.as_str()));
            remote_target
                && (matches!(app.connectivity.state, S::Offline | S::Reconnecting)
                    || app
                        .effective_device_id()
                        .is_some_and(|id| !app.device_online(&id, chrono::Utc::now())))
        }
    };
    let offline = app.connectivity.state == S::Offline;
    degraded.then(|| {
        (
            if offline {
                "Offline — messages will send when you're back online."
            } else {
                "Messages will send once the connection recovers."
            }
            .to_string(),
            offline,
        )
    })
}

fn queue_notice_line(theme: &Theme, notice: &str, offline: bool) -> El {
    // Not a warning box: one quiet caption line, amber dot only for hard
    // offline; it clears itself the moment the path heals.
    let dot = if offline {
        theme.warning
    } else {
        theme.text_faint
    };
    row()
        .key("composer-queue-notice")
        .mx(8.0)
        .mt(6.0)
        .gap(6.0)
        .center_y()
        .child(el().size(5.0, 5.0).radius(2.5).fill(dot))
        .child(
            text(notice.to_string())
                .font(style(11.0, 400.0).lh(14.0))
                .color(theme.text_faint)
                .min_w(0.0)
                .truncate(),
        )
}

// ---------------------------------------------------------------------------
// Attachment strip
// ---------------------------------------------------------------------------

fn render_comments_chip(theme: &Theme, count: usize) -> Option<El> {
    if count == 0 {
        return None;
    }
    Some(
        row()
            .key("composer-comments")
            .px(attachments::STRIP_PAD_X)
            .pt(attachments::STRIP_PAD_TOP)
            .child(
                row()
                    .h(attachments::COMMENT_BADGE_HEIGHT)
                    .px(8.0)
                    .gap(5.0)
                    .center_y()
                    .radius(6.0)
                    .fill(theme.ink(0.05))
                    .child(icon(&icons::CHAT_ROUND_LINE, 13.0).color(theme.text_muted))
                    .child(
                        text(crate::comments::chip_label(count))
                            .font(style(11.5, 500.0))
                            .color(theme.text_muted),
                    ),
            ),
    )
}

fn render_attachment_strip(app: &App, theme: &Theme) -> Option<El> {
    let staged = app.composer.staged();
    if staged.is_empty() {
        return None;
    }
    let mut strip = row()
        .key("composer-attachments")
        .w_full()
        .flex_wrap()
        .gap(attachments::STRIP_GAP)
        .px(attachments::STRIP_PAD_X)
        .pt(attachments::STRIP_PAD_TOP);
    for (ix, att) in staged.iter().enumerate() {
        strip = strip.child(attachment_thumb(app, theme, att, ix));
    }
    Some(strip)
}

fn attachment_thumb(app: &App, theme: &Theme, att: &StagedAttachment, ix: usize) -> El {
    let image = app.composer.thumbnail(att);
    let preview = PreviewImage {
        name: att.name.clone(),
        image: image.clone(),
    };
    let preview = std::rc::Rc::new(preview);
    let remove_id = att.id.clone();
    let thumb = el()
        .key(format!("composer-att-thumb-{ix}"))
        .size(attachments::STRIP_THUMB, attachments::STRIP_THUMB)
        .radius(8.0)
        .border(1.0, theme.hairline(0.10))
        .cursor(rune::Cursor::Pointer)
        .clip_rounded(8.0)
        .on_press(move |app: &mut App| {
            app.composer.preview = Some((*preview).clone());
        })
        .child(
            el().size(
                attachments::STRIP_THUMB - 2.0,
                attachments::STRIP_THUMB - 2.0,
            )
            .radius(7.0)
            .pixels_fit(image.pixels(), rune::SurfaceFit::Fill),
        );
    let remove = el()
        .key(format!("composer-att-remove-{ix}"))
        .size(18.0, 18.0)
        .radius(9.0)
        .center()
        .fill(theme.bg)
        .shadow(1.0, 4.0, theme.ink(0.20))
        .cursor(rune::Cursor::Pointer)
        .group_opacity(0.0, 1.0)
        .on_press(move |app: &mut App| {
            super::remove_attachment(app, &remove_id);
        })
        .child(icon(&icons::CLOSE_CIRCLE, 14.0).color(theme.text_muted));

    el().hover_group()
        .child(thumb)
        .child(remove.pin(None, Some(-6.0), Some(-6.0), None))
}

// ---------------------------------------------------------------------------
// Completion popups
// ---------------------------------------------------------------------------

fn mention_popup(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    let Some(token) = app.composer.mention.token.as_ref() else {
        return;
    };
    let state = &app.composer.mention;
    let empty = if let Some(error) = &state.error {
        error.clone()
    } else if state.loading && state.results.is_empty() {
        "Searching…".to_string()
    } else if token.query.is_empty() {
        "No files available".to_string()
    } else {
        "No matching files".to_string()
    };
    let items: Vec<PopupItem> = state
        .results
        .iter()
        .map(|result| {
            let (directory, name) = match result.path.rsplit_once('/') {
                Some((directory, name)) => (directory.to_string(), name.to_string()),
                None => (String::new(), result.path.clone()),
            };
            PopupItem::new(result.path.clone(), name)
                .description(directory)
                .leading(
                    icon(
                        if result.is_dir {
                            &icons::FOLDER
                        } else {
                            &icons::DOCUMENT
                        },
                        14.0,
                    )
                    .color(theme.text_muted),
                )
        })
        .collect();
    PopupList::<App>::new("composer-mention", INPUT_KEY)
        .side(Side::Top)
        .width(320.0)
        .max_height(320.0)
        .open(true)
        .items(items)
        .active(state.active)
        .empty_message(empty)
        .label("File mentions")
        .on_active(|app: &mut App, index: usize| app.composer.mention.active = Some(index))
        .on_choose(|app: &mut App, id: &str, _tx: &mut Tx| mentions::accept_mention_path(app, id))
        .on_close(|app: &mut App| mentions::dismiss_mention(app))
        .build(&theme.kit, layer);
}

fn slash_popup(app: &App, theme: &Theme, layer: &mut Vec<El>) {
    let state = &app.composer.slash;
    if state.token.is_none() {
        return;
    }
    let commands = state
        .harness
        .and_then(|h| app.composer.slash_cache.get(&h))
        .map(Vec::as_slice)
        .unwrap_or_default();
    let empty = if let Some(error) = &state.error {
        error.clone()
    } else if state.loading && commands.is_empty() {
        "Loading commands…".to_string()
    } else if commands.is_empty() {
        "This agent has no slash commands".to_string()
    } else {
        "No matching commands".to_string()
    };
    let items: Vec<PopupItem> = state
        .filtered
        .iter()
        .filter_map(|&ix| commands.get(ix))
        .map(|command| {
            let mut description = command.description.clone();
            if let Some(hint) = &command.input_hint {
                description = if description.is_empty() {
                    format!("<{hint}>")
                } else {
                    format!("{description} · <{hint}>")
                };
            }
            PopupItem::new(command.name.clone(), format!("/{}", command.name))
                .description(description)
                .leading(icon(&icons::COMMAND, 14.0).color(theme.text_muted))
        })
        .collect();
    PopupList::<App>::new("composer-slash", INPUT_KEY)
        .side(Side::Top)
        .width(380.0)
        .max_height(280.0)
        .open(true)
        .items(items)
        .active(state.active)
        .empty_message(empty)
        .label("Slash commands")
        .on_active(|app: &mut App, index: usize| app.composer.slash.active = Some(index))
        .on_choose(|app: &mut App, id: &str, _tx: &mut Tx| slash::accept_slash_name(app, id))
        .on_close(|app: &mut App| slash::dismiss_slash(app))
        .build(&theme.kit, layer);
}

/// The `Placeholder` copy re-exported for the shell's focus/status copy.
pub fn placeholder(app: &App) -> &str {
    &app.composer.placeholder
}

/// The `Placeholder::COMPOSER` default (used by the shell's boot state).
pub fn default_placeholder() -> &'static str {
    Placeholder::COMPOSER
}

/// Keep a reference to the composer state type for docs/tests.
pub fn state(app: &App) -> &ComposerState {
    &app.composer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_key_is_prefixed() {
        assert!(INPUT_KEY.starts_with("composer/"));
    }

    #[test]
    fn placeholder_copy_matches_the_reference() {
        assert_eq!(Placeholder::COMPOSER, "Do anything…");
        assert!(Placeholder::WIZARD.contains("pick an option above"));
    }

    #[test]
    fn kind_is_exported_for_the_shell() {
        // The picker kinds the cluster triggers are the pickers module's.
        let _ = crate::pickers::PickerKind::HarnessModel;
    }
}
