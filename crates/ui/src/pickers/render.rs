//! Rendering for the picker chrome: trigger chips, footer, popover frames, the
//! harness/model list with its scrollbar, and the branch/checkout/traits menus.

use super::*;

const MODEL_SCROLLBAR_TRACK_INSET: f32 = 4.0;
const MODEL_SCROLLBAR_HIT_WIDTH: f32 = 10.0;
const MODEL_SCROLLBAR_THUMB_WIDTH: f32 = 3.0;
const MODEL_SCROLLBAR_HOVER_THUMB_WIDTH: f32 = 5.0;
const MODEL_SCROLLBAR_MIN_THUMB: f32 = 24.0;
impl Pickers {
    // ---- render ----

    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Traits => "picker-traits",
            PickerKind::Mode => "picker-mode",
            PickerKind::Space => "picker-space",
            PickerKind::Device => "picker-device",
        };
        let open = self.open_kind() == Some(kind);
        // Ghost pill (hearth composer/styles.tsx `pill`): `h-8 rounded-lg px-2.5
        // gap-1.5 text-[12px] font-medium text-muted-foreground`, icons size-4,
        // hover/open wash — no border, no caret; the actions row stays quiet.
        let compact_traits = kind == PickerKind::Traits && self.compact_traits;
        div()
            .id(id)
            .h(px(32.0))
            .max_w(px(208.0))
            // Shrinkable under row pressure — four footer chips share one
            // line; without min_w_0 they overflowed and painted overlapped.
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            // hearth composer/styles.tsx `pill`: `transition-colors` — the wash
            // and text brighten fade over 150ms.
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when_some(chip_icon, |el, (path, tint)| {
                el.child(
                    crate::icons::icon(path)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
            })
            .when(!compact_traits, |el| {
                el.child(div().min_w_0().truncate().child(label))
            })
            .when(compact_traits, |el| {
                el.w(px(32.0))
                    .px_0()
                    .justify_center()
                    .child(div().text_size(px(11.0)).child("•••"))
            })
    }

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            // Four of these share one row now (device, project, checkout,
            // ref): cap each early and let them SHRINK (`min_w_0`) — without
            // it the clusters overflowed into each other and the labels
            // painted overlapped (user report).
            .max_w(px(160.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    /// The new-session canvas's target row — device + project selector chips
    /// under the canvas logo (their popovers anchor BELOW; the composer
    /// footer carries only checkout + ref now, and sessions show their
    /// target in the titlebar instead).
    pub fn render_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            Some(PickerKind::Device) => {
                let content = self.render_device_popover(cx);
                Some((PickerKind::Device, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let (device_label, project_label, offline) = {
            let state = self.state.read(cx);
            let device_id = state.effective_device_id();
            let device_label: SharedString = device_id
                .as_deref()
                .and_then(|id| state.device_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| "This device".to_string())
                .into();
            let offline = device_id
                .as_deref()
                .is_some_and(|id| !state.device_online(id, chrono::Utc::now()));
            let project_label: SharedString = state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No project".to_string())
                .into();
            (device_label, project_label, offline)
        };
        let device_chip = self
            .footer_chip(
                PickerKind::Device,
                "picker-device",
                crate::icons::MONITOR,
                device_label,
                &theme,
                cx,
            )
            .when(offline, |el| el.text_color(theme.warning.opacity(0.8)));
        let project_chip = self.footer_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::FOLDER,
            project_label,
            &theme,
            cx,
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(attach_overlay_below(
                device_chip,
                &mut overlay,
                PickerKind::Device,
                "device-popover",
                closing,
            ))
            .child(attach_overlay_below(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .into_any_element()
    }

    /// The composer footer row: checkout-kind + ref, LEFT-aligned, only when
    /// the picked (or session's) project has git. Device + project moved to
    /// the new-session canvas ([`Self::render_target_selectors`]); sessions
    /// name their target in the titlebar.
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty locked state.
        let (space, session, change_request) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            let change_request = session
                .as_ref()
                .and_then(|chat| state.change_request_for_chat(chat).cloned());
            (space, session, change_request)
        };
        let row = || {
            // Symmetric: the container's 8px gap sits above the toolbar;
            // bleeding 8 of the container's 16px bottom padding (mb -8)
            // leaves 8 below — equal air on both sides of the row.
            // `w_full` is load-bearing: without it the canvas layout sizes
            // the row to CONTENT, and the left cluster's flex_1 (basis 0)
            // collapsed to zero width — both clusters painted from the same
            // origin, chips overlapping (user report).
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .mb(px(-8.0))
        };

        if let Some(chat) = &session {
            // Sessions never move: read-only checkout-kind + ref labels,
            // LEFT-aligned, only when the session's project has git. The
            // target (project @ device) lives in the titlebar now.
            let space = space.as_ref().filter(|s| s.git_detected)?;
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let (icon_path, label) = if is_worktree {
                (crate::icons::FOLDER_WITH_FILES, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            // Mirrors the draft chips: checkout hugs the left edge, ref the
            // right.
            let left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(label),
                    &theme,
                ));
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_w_0()
                .when_some(change_request, |el, summary| {
                    el.child(crate::change_requests::pull_request_badge(
                        "composer-pull-request".into(),
                        summary,
                        crate::change_requests::ChangeRequestBadgeSurface::Composer,
                        &theme,
                    ))
                })
                .child(Self::footer_label(
                    crate::icons::GIT_BRANCH,
                    chat.branch
                        .clone()
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from("No ref")),
                    &theme,
                ));
            return Some(row().child(left).child(right).into_any_element());
        }

        // New-session canvas: checkout + ref only, LEFT-aligned (device +
        // project live under the canvas logo now).
        let git = space.as_ref().is_some_and(|s| s.git_detected);
        if !git {
            return None;
        }
        // Refs feed the draft labels — eager + idempotent.
        self.ensure_refs(false, cx);
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            // Space/Device popovers mount on the canvas selectors
            // (`render_target_selectors`), not here.
            _ => None,
        };

        let ref_label = self.ref_label();
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        // Checkout on the left edge, ref on the right — the row's
        // justify_between splits them (user request).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
                closing,
            ));
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay_end(
                ref_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
                closing,
            ));
        Some(row().child(left).child(right).into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            // hearth caps its tallest picker at min(640px, 75vh).
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    /// [`Self::popover_frame`] without the p-1 inset — the harness/model
    /// picker's rail + list panes bleed to the card edge (hearth
    /// harness-model-picker.tsx `className="w-80 p-0"`).
    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    pub(super) fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel | PickerKind::Traits => {
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.catalog_rev += 1;
                            this.ensure_harnesses(false, cx);
                        }
                        // Projects/devices load nothing; no retry surface exists.
                        PickerKind::Space | PickerKind::Device | PickerKind::Mode => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The virtualized list's plain scroll handle (bounds/offset for the
    /// floating scrollbar; `UniformList` tracks it internally).
    pub(super) fn model_scroll_base(&self) -> gpui::ScrollHandle {
        self.model_scroll.0.borrow().base_handle.clone()
    }

    fn model_scrollbar_metrics(&self) -> Option<ModelScrollbarMetrics> {
        let scroll = self.model_scroll_base();
        let bounds = scroll.bounds();
        let viewport_height = f32::from(bounds.size.height);
        // GPUI stores the maximum as a positive distance; only the live
        // scroll offset is negative while content moves upward.
        let max_scroll = f32::from(scroll.max_offset().y).max(0.0);
        if viewport_height <= 0.0 || max_scroll <= 0.0 {
            return None;
        }
        let track_height = (viewport_height - MODEL_SCROLLBAR_TRACK_INSET * 2.0).max(0.0);
        if track_height <= 0.0 {
            return None;
        }
        let content_height = viewport_height + max_scroll;
        let thumb_height = (track_height * viewport_height / content_height)
            .max(MODEL_SCROLLBAR_MIN_THUMB)
            .min(track_height);
        let current_scroll = (-f32::from(scroll.offset().y)).clamp(0.0, max_scroll);
        let travel = (track_height - thumb_height).max(0.0);
        Some(ModelScrollbarMetrics {
            track_height,
            thumb_top: travel * current_scroll / max_scroll,
            thumb_height,
            max_scroll,
        })
    }

    fn model_scrollbar_to_pointer(
        &mut self,
        pointer_y: gpui::Pixels,
        grab_offset: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(metrics) = self.model_scrollbar_metrics() else {
            return;
        };
        let scroll = self.model_scroll_base();
        let bounds = scroll.bounds();
        let pointer_in_track = f32::from(pointer_y - bounds.top()) - MODEL_SCROLLBAR_TRACK_INSET;
        let thumb_top = (pointer_in_track - grab_offset).clamp(0.0, metrics.travel());
        let scroll_to = if metrics.travel() <= 0.0 {
            0.0
        } else {
            thumb_top / metrics.travel() * metrics.max_scroll
        };
        let offset = scroll.offset();
        scroll.set_offset(gpui::Point::new(offset.x, px(-scroll_to)));
        cx.notify();
    }

    fn on_model_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_list_hovered == *hovered {
            return;
        }
        self.model_list_hovered = *hovered;
        if !*hovered && self.model_scrollbar_drag.is_none() {
            self.model_scrollbar_hovered = false;
        }
        cx.notify();
    }

    fn on_model_scrollbar_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        // Keep the active treatment while a captured drag travels outside the
        // model list; the hover callback quite correctly turns false there.
        let active = *hovered || self.model_scrollbar_drag.is_some();
        if self.model_scrollbar_hovered != active {
            self.model_scrollbar_hovered = active;
            cx.notify();
        }
    }

    fn on_model_scrollbar_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(metrics) = self.model_scrollbar_metrics() else {
            return;
        };
        window.focus(&self.focus, cx);
        let bounds = self.model_scroll_base().bounds();
        let pointer_in_track =
            f32::from(event.position.y - bounds.top()) - MODEL_SCROLLBAR_TRACK_INSET;
        let grab_offset = if (metrics.thumb_top..=metrics.thumb_top + metrics.thumb_height)
            .contains(&pointer_in_track)
        {
            pointer_in_track - metrics.thumb_top
        } else {
            metrics.thumb_height / 2.0
        };
        self.model_scrollbar_drag = Some(ModelScrollbarGrab { grab_offset });
        self.model_scrollbar_to_pointer(event.position.y, grab_offset, cx);
        cx.stop_propagation();
    }

    fn on_model_scrollbar_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<ModelScrollbarDrag>,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.model_scrollbar_drag else {
            return;
        };
        self.model_scrollbar_to_pointer(event.event.position.y, drag.grab_offset, cx);
    }

    fn on_model_scrollbar_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.model_scrollbar_drag = None;
        if !self.model_list_hovered {
            self.model_scrollbar_hovered = false;
        }
        cx.notify();
    }

    fn render_model_scrollbar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dragging = self.model_scrollbar_drag.is_some();
        if !self.model_list_hovered && !dragging {
            return None;
        }
        let metrics = self.model_scrollbar_metrics()?;
        let active = self.model_scrollbar_hovered || dragging;
        let thumb_width = if active {
            MODEL_SCROLLBAR_HOVER_THUMB_WIDTH
        } else {
            MODEL_SCROLLBAR_THUMB_WIDTH
        };
        Some(
            div()
                .id("model-scrollbar")
                .absolute()
                .top(px(0.0))
                .bottom(px(0.0))
                .right(px(0.0))
                .w(px(MODEL_SCROLLBAR_HIT_WIDTH))
                .on_hover(cx.listener(Self::on_model_scrollbar_hover))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_down),
                )
                .on_drag(ModelScrollbarDrag, |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| ModelScrollbarDragGhost)
                })
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(MODEL_SCROLLBAR_TRACK_INSET + metrics.thumb_top))
                        .right(px(2.0))
                        // The thumb is an absolute child inside a fixed-width
                        // hit rail, so hover expansion never reflows rows.
                        .w(px(thumb_width))
                        .h(px(metrics.thumb_height))
                        .rounded(px(thumb_width / 2.0))
                        .bg(theme.text_faint.opacity(if active { 0.68 } else { 0.5 })),
                )
                .into_any_element(),
        )
    }

    /// The ref picker (t3code BranchToolbarBranchSelector): search on top,
    /// rows with right-aligned muted `current`/`worktree` tags, and a
    /// "Showing X of Y refs" footer when the list is capped.
    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No project selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        // Existing session: the highlighted row is the SESSION's branch and a
        // pick switches the checkout (see `pick_ref`); a new chat highlights
        // the draft pick.
        let session_branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone());
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    let selected = session_branch.or_else(|| self.config.branch.clone());
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                // Right-aligned muted tag (t3code `text-[10px]
                                // text-muted-foreground/45`): current beats worktree.
                                let tag: Option<&'static str> = if row.current {
                                    Some("current")
                                } else if row.worktree_path.is_some() {
                                    Some("worktree")
                                } else {
                                    None
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        // Mid-session switch failure (dirty tree, ref checked out elsewhere):
        // git's own message, under a hairline.
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    /// The checkout-kind dropdown (t3code BranchToolbarEnvModeSelector): two
    /// rows — "Current checkout"/"Current worktree" (local) and "New worktree".
    fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let local_label: &'static str = if has_worktree {
            "Current worktree"
        } else {
            "Current checkout"
        };
        let local_icon = if has_worktree {
            crate::icons::FOLDER_WITH_FILES
        } else {
            crate::icons::FOLDER
        };
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (CheckoutKind::Local, local_label, local_icon),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The combined harness + model switcher (hearth harness-model-picker.tsx):
    /// a vertical harness rail of square brand-icon tabs on the left, the
    /// viewed harness's models on the right. On an existing chat the other
    /// tabs stay visible but disabled — the lock reads as a rule.
    /// The harness/model picker (t3code ModelPickerContent): an icons-only
    /// harness rail on the left (favorites star on top), a search box over
    /// the model list on the right. Rows are two lines — model name over the
    /// harness icon + name (t3 `showProvider`, replacing the description) —
    /// with a ⌘N jump chip and a star toggle trailing. Searching hides the
    /// rail and spans every harness.
    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        const HEIGHT: f32 = 346.0; // t3 max-h-86.5

        let theme = Theme::of(cx).clone();

        // Catalog-level loading/error take over the whole card.
        match &self.harnesses {
            Loadable::Loading | Loadable::Idle => {
                return div()
                    .h(px(HEIGHT))
                    .p(px(8.0))
                    .child(popover::skeleton_rows(
                        "harness-skeleton",
                        &theme,
                        4,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Error(message) => {
                let message = message.clone();
                return div()
                    .h(px(HEIGHT))
                    .p(px(8.0))
                    .child(self.retry_row(
                        "harness-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Ready(_) => {}
        }

        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        let favorites_view = !searching && self.model_rail == ModelRail::Favorites;
        let descriptors = self.rail_descriptors(cx);
        // No-agents empty state: the catalog loaded but offers nothing
        // runnable (every enabled harness is missing its CLI, or nothing is
        // enabled) and there's no committed chat harness to force-include —
        // guidance instead of an empty rail.
        if descriptors.is_empty() {
            return div()
                .p(px(16.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::TERMINAL)
                        .size(px(20.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from("No agents available")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .text_center()
                        .child(SharedString::from(
                            "Enable an installed agent in Settings → Agents, \
                             or install an agent CLI.",
                        )),
                )
                .into_any_element();
        }
        let rows = self.model_rows(cx);

        // ── rail: icons only (t3 ModelPickerSidebar) — the favorites star,
        //    a divider, one brand icon per harness. The selected tab wears a
        //    3px accent bar hugging the rail's right edge. Hidden while a
        //    search is live (the query spans every harness).
        let rail: Option<AnyElement> = (!searching).then(|| {
            let mut column = div()
                .w(px(44.0))
                .flex_none()
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(4.0));
            column = column.child(
                div()
                    .id("model-rail-favorites")
                    .relative()
                    .w(px(36.0))
                    .h(px(36.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(!favorites_view, |el| {
                        el.hover(|s| s.bg(crate::theme::ink(0.06)))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_rail = ModelRail::Favorites;
                        // Anchor on the selected row when it's starred, else
                        // the top — never a stray second highlight.
                        this.active = this.selected_model_index(cx);
                        this.model_scroll_base().set_offset(gpui::Point::default());
                        this.model_scroll
                            .scroll_to_item(this.active, gpui::ScrollStrategy::Nearest);
                        cx.notify();
                    }))
                    .child(
                        crate::icons::icon(crate::icons::STAR_BOLD)
                            .size(px(17.0))
                            .text_color(if favorites_view {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.75)
                            }),
                    )
                    .when(favorites_view, |el| {
                        el.child(rail_indicator(picker_purple(&theme)))
                    }),
            );
            // Full-bleed divider, aligned with the search row's bottom
            // hairline (see the height math there) — one line across.
            column = column.child(
                div()
                    .h(px(1.0))
                    .mx(px(-4.0))
                    .my(px(1.0))
                    .bg(crate::theme::hairline(0.08)),
            );
            for (ix, descriptor) in descriptors.iter().enumerate() {
                let harness = descriptor.id;
                let is_viewed = !favorites_view && effective == Some(harness);
                let is_disabled = locked && effective != Some(harness);
                let (icon_path, tint) = harness_brand_icon(harness);
                column = column.child(
                    div()
                        .id(("harness-tab", ix))
                        .relative()
                        .w(px(36.0))
                        .h(px(36.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(is_disabled, |el| el.opacity(0.35))
                        .when(!is_disabled, |el| el.cursor_pointer())
                        .when(!is_disabled && !is_viewed, |el| {
                            el.hover(|s| s.bg(crate::theme::ink(0.06)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.model_rail = ModelRail::Harness;
                            this.pick_harness(harness, cx);
                            cx.notify();
                        }))
                        .child(crate::icons::icon(icon_path).size(px(18.0)).text_color(
                            tint.unwrap_or(if is_viewed {
                                theme.text
                            } else {
                                theme.text_muted
                            }),
                        ))
                        .when(is_viewed, |el| {
                            el.child(rail_indicator(picker_purple(&theme)))
                        }),
                );
            }
            column.into_any_element()
        });

        // ── search row: icon + borderless input over a FULL-BLEED hairline
        //    (it meets the rail's divider at the same y, one line across the
        //    card — user request; no accent tint). Height matches the rail's
        //    star tab band exactly: 4px pad + 36px tab + 4px gap + 1px
        //    divider margin = the hairline at y 45–46, same as this row's
        //    inside-drawn bottom border at h 46.
        let search_row = div()
            .flex_none()
            .h(px(46.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                crate::icons::icon(crate::icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .child(self.search.clone()),
            );

        // ── model rows: a VIRTUALIZED uniform list — only the visible slice
        //    renders, so a 7k-model catalog scrolls as smoothly as seven
        //    (field report: the un-virtualized stack was the picker's lag).
        //    Keyboard nav scrolls via the UniformListScrollHandle.
        let effective_models = effective.and_then(|h| self.models.get(&h));
        let model_list: Option<AnyElement> = if !rows.is_empty() {
            let entity = cx.entity();
            let row_data = rows.clone();
            Some(
                gpui::uniform_list(
                    "model-menu-scroll",
                    rows.len(),
                    move |range, _window, app| {
                        entity.update(app, |this, cx| {
                            range
                                .filter_map(|ix| {
                                    row_data
                                        .get(ix)
                                        .map(|row| this.render_model_row(ix, row, cx))
                                })
                                .collect::<Vec<AnyElement>>()
                        })
                    },
                )
                .size_full()
                .px(px(6.0))
                .track_scroll(&model_scroll)
                .into_any_element(),
            )
        } else {
            None
        };
        let list_children: Vec<AnyElement> = if !rows.is_empty() {
            Vec::new()
        } else if searching {
            vec![empty_list_note(&theme, "No models found")]
        } else if favorites_view {
            vec![empty_list_note(
                &theme,
                "No starred models yet — hit a row's star",
            )]
        } else {
            match effective_models {
                Some(Loadable::Error(message)) => {
                    let message = message.clone();
                    vec![self.retry_row(
                        "model-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    )]
                }
                _ => vec![popover::skeleton_rows(
                    "model-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    cx,
                )],
            }
        };

        let model_scrollbar = self.render_model_scrollbar(&theme, cx);
        let pane = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            // A whisper of wash lifts the pane off the rail (t3
            // `bg-muted/40` + `border-l border-border/70`).
            .bg(crate::theme::ink(0.02))
            .when(rail.is_some(), |el| {
                el.border_l_1().border_color(crate::theme::hairline(0.07))
            })
            .child(search_row)
            .child(
                div()
                    .id("model-list-scroll-host")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .py(px(6.0))
                    .on_hover(cx.listener(Self::on_model_list_hover))
                    .child(match model_list {
                        Some(list) => list,
                        // Empty/loading/error notes: a plain static stack.
                        None => div()
                            .id("model-menu-scroll")
                            .size_full()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .px(px(6.0))
                            .children(list_children)
                            .into_any_element(),
                    })
                    // Absolute child: the hit rail and thumb float above the
                    // scroll content without consuming any list width.
                    .children(model_scrollbar),
            );

        div()
            .h(px(HEIGHT))
            .flex()
            .flex_row()
            .items_stretch()
            .children(rail)
            .child(pane)
            .into_any_element()
    }

    /// One model row for the virtualized list. `ix` is the row's GLOBAL index
    /// (⌘N chips, hover-cursor, and activation all key on it). The 2px
    /// inter-row gap is baked into each item's bottom padding so every item
    /// is the same height (uniform_list measures the first).
    fn render_model_row(
        &mut self,
        ix: usize,
        row: &ModelRowData,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let effective = self.effective_harness(cx);
        let is_selected = Some(row.harness) == effective
            && self.selected_model(cx).map(|m| m.id.as_str()) == Some(row.model.id.as_str());
        let is_active = ix == self.active;
        let is_fav = self.defaults.is_favorite(row.harness, &row.model.id);
        let (icon_path, tint) = harness_brand_icon(row.harness);
        let label: SharedString = row.model.label.clone().into();
        let harness_name = row.harness_name.clone();
        let harness = row.harness;
        let star_model = row.model.id.clone();
        // Provider attribution (field report: several connected opencode
        // providers advertise identically-named models — "GLM-5.2" exists
        // under 64 providers — and rows were indistinguishable). The driver
        // ships the provider display name in `description`; other harnesses'
        // taglines read fine in the same slot. Skip when it just repeats the
        // harness name.
        let attribution: Option<SharedString> = row
            .model
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(harness_name.as_ref()))
            .map(|d| SharedString::from(d.to_owned()));
        let mut el = div()
            .id(("model-row", ix))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer();
        // ONE moving highlight (t3/Base-UI combobox): hovering moves the
        // keyboard cursor instead of painting its own wash, so hover + arrow
        // cursor can never wear two washes at once. Selection is the
        // distinct stronger treatment (wash + ring).
        if is_selected {
            el = el
                .bg(crate::theme::card_selected_bg())
                .shadow(crate::theme::card_selected_shadows());
        } else if is_active {
            el = el.bg(crate::theme::ink(0.05));
        }
        el = el.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.active != ix {
                this.active = ix;
                cx.notify();
            }
        }));
        el = el
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_model_index(ix, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(label),
                    )
                    .child(
                        // Harness identity subline (t3 `showProvider`), plus
                        // the model's own attribution when it carries one.
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                crate::icons::icon(icon_path)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7))),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(harness_name),
                            )
                            .when_some(attribution, |el, attribution| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from("·")),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(attribution),
                                )
                            }),
                    ),
            );
        if ix < 9 {
            el = el.child(popover::kbd_hint(&theme, &format!("⌘{}", ix + 1)));
        }
        el = el.child(
            div()
                .id(("model-star", ix))
                .flex_none()
                .w(px(22.0))
                .h(px(22.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_model_favorite(harness, &star_model, cx);
                }))
                .child(
                    crate::icons::icon(if is_fav {
                        crate::icons::STAR_BOLD
                    } else {
                        crate::icons::STAR
                    })
                    .size(px(13.0))
                    .text_color(if is_fav {
                        theme.warning
                    } else {
                        theme.text_muted.opacity(0.45)
                    }),
                ),
        );
        div().pb(px(2.0)).child(el).into_any_element()
    }

    /// The traits dropdown body (t3code TraitsPicker): the reasoning ladder
    /// plus every advertised model option as headed sections of menu ROWS —
    /// label, a "Default" badge on the section's default choice, and the
    /// trailing check on the selected row. Sections split by hairline
    /// separators. Selecting keeps the menu open for multi-adjust.
    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_rows("traits-skeleton", &theme, 3, cx.entity_id(), cx);
        };
        let levels = self.trait_ladder(cx);
        // Display the effective level (draft pick or the chat's config), so
        // the ladder check mirrors the chip summary.
        let current = self.effective_reasoning(cx);

        let mut sections: Vec<AnyElement> = Vec::new();
        if !levels.is_empty() {
            let default_level = default_reasoning(&levels);
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    // 2px row gap — the menu-column rhythm everywhere else
                    // (model list, device switcher); without it adjacent
                    // hover/selected washes fuse into one blob (user report).
                    .gap(px(2.0))
                    .child(popover::menu_heading(&theme, "Reasoning"))
                    .children(levels.into_iter().enumerate().map(|(ix, level)| {
                        let is_active = current == Some(level);
                        let is_default = default_level == Some(level);
                        let mut row =
                            popover::menu_row(&theme, is_active, format!("trait-reasoning-{ix}"))
                                .id(("reasoning-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_reasoning(level, cx);
                                }))
                                .child(SharedString::from(reasoning_label(level)));
                        row = row.child(div().flex_1());
                        if is_default {
                            row = row.child(default_badge(&theme));
                        }
                        row
                    }))
                    .into_any_element(),
            );
        }

        let selections = self.explicit_options(cx);
        for (opt_ix, option) in model.options.iter().enumerate() {
            if !sections.is_empty() {
                sections.push(popover::menu_separator().into_any_element());
            }
            let selected_choice = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .unwrap_or(&option.default_choice)
                .to_string();
            let option_id = option.id.clone();
            let default_choice = option.default_choice.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0)) // same rhythm as the Reasoning section above
                    .child(popover::menu_heading(&theme, &option.label))
                    .children(
                        option
                            .choices
                            .iter()
                            .enumerate()
                            .map(|(choice_ix, choice)| {
                                let is_active = selected_choice == choice.id;
                                let choice_id = choice.id.clone();
                                let option_id = option_id.clone();
                                let is_default = choice.id == default_choice;
                                let mut row = popover::menu_row(
                                    &theme,
                                    is_active,
                                    format!("trait-choice-{opt_ix}-{choice_ix}"),
                                )
                                .id(("trait-choice", opt_ix * 32 + choice_ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_option(
                                        option_id.clone(),
                                        choice_id.clone(),
                                        is_default,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from(choice.label.clone()));
                                row = row.child(div().flex_1());
                                if is_default {
                                    row = row.child(default_badge(&theme));
                                }
                                row
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .pb(px(2.0))
            .children(sections)
            .into_any_element()
    }

    /// The interaction-mode dropdown body (plan/agent/chat): one menu row per
    /// mode with a trailing check on the active one. Selecting closes the
    /// menu (a single choice, unlike the multi-adjust traits menu).
    fn render_mode_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let current = self.effective_mode(cx);
        let modes = [SessionMode::Plan, SessionMode::Agent, SessionMode::Chat];
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(&theme, "Mode"))
            .children(modes.into_iter().enumerate().map(|(ix, mode)| {
                let is_active = current == Some(mode);
                popover::menu_row(&theme, is_active, format!("mode-row-{ix}"))
                    .id(("mode-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_mode(mode, cx);
                        this.animate_close(cx);
                    }))
                    .child(SharedString::from(mode_label(mode)))
            }))
            .into_any_element()
    }
}

/// The "Default" marker beside a section's default choice: a ghost badge —
/// bare muted text, no border or fill (user request; t3code draws an outline
/// pill here).
fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from("Default"))
}

/// The 3px bar marking the selected rail tab (t3 ModelPickerSidebar
/// `SELECTED_INDICATOR_CLASS`, `rounded-l-full`): LEFT half-capsule only —
/// the flat right edge presses against the rail/pane border it hugs.
fn rail_indicator(tint: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .right(px(-4.0))
        .top(px(8.0))
        .w(px(3.0))
        .h(px(20.0))
        .rounded_tl(px(3.0))
        .rounded_bl(px(3.0))
        .bg(tint)
}

/// The picker's selection purple — the app's violet identity (the "nice
/// purple" family inline code wears), NOT the indigo `accent`: the indigo
/// bar read blue against the glass (user request). violet-400 on dark,
/// violet-600 on light (AA against white).
fn picker_purple(theme: &Theme) -> gpui::Hsla {
    match theme.appearance {
        crate::theme::Appearance::Dark => crate::theme::oklch(0.702, 0.183, 293.541),
        crate::theme::Appearance::Light => crate::theme::oklch(0.541, 0.281, 293.009),
    }
}

/// Centered muted note filling an empty model list ("No models found").
fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted.opacity(0.6))
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}

/// Attach the (single) open popover overlay to its trigger chip.
fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element, closing));
    }
    chip
}

/// [`attach_overlay`] opening DOWNWARD — the canvas target selectors sit
/// mid-screen, so their menus drop below the chips.
fn attach_overlay_below(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_below(id, element, closing));
    }
    chip
}

/// [`attach_overlay`] with the menu RIGHT-ALIGNED to the trigger (t3code
/// `align="end"` — right-edge triggers like the ref picker open leftward).
fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element, closing));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // A HEARTH_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open_kind() {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        // Eager-load the harness catalog + every offered harness's models so
        // the chip reads "Fable 5" (a concrete pick) before any popover
        // opens, and rail switches inside the picker are instant.
        self.ensure_harnesses(false, cx);
        self.prefetch_models(false, cx);
        // A popover opened data-side (HEARTH_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open_kind(),
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (hearth `modelText`); the
        // harness reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        // No-agents state: nothing runnable resolved (and the catalog is
        // loaded, so that's a conclusion, not a loading gap) — the chip says
        // so instead of wearing a brand mark for an agent that can't run.
        let no_agents = self.no_agents_available() && self.effective_harness(cx).is_none();
        let model_label: SharedString = if no_agents {
            SharedString::from("No agents available")
        } else {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        let harness_icon: (&'static str, Option<gpui::Hsla>) = match self.effective_harness(cx) {
            Some(harness) => harness_brand_icon(harness),
            None if no_agents => (crate::icons::TERMINAL, Some(theme.text_muted)),
            None => (crate::icons::HUGINN_MARK, None),
        };
        let explicit_options = self.explicit_options(cx);
        let traits_set = traits_summary(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &explicit_options,
        );
        let traits_active = traits_customized(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &self.trait_ladder(cx),
            &explicit_options,
        );
        let traits_label: SharedString = traits_set
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Traits"));

        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the composer FOOTER row (see
        // `render_footer`), not here.
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            // Footer-row pickers — their popovers mount down there.
            Some(PickerKind::Branch)
            | Some(PickerKind::Checkout)
            | Some(PickerKind::Space)
            | Some(PickerKind::Device) => None,
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    // t3 ModelPickerContent `max-w-90` — 360px.
                    self.popover_frame_flush(360.0, content, cx),
                ))
            }
            Some(PickerKind::Traits) => {
                let content = div()
                    .p(px(4.0))
                    .child(self.render_traits_sections(cx))
                    .into_any_element();
                Some((
                    PickerKind::Traits,
                    self.popover_frame_flush(240.0, content, cx),
                ))
            }
            Some(PickerKind::Mode) => {
                let content = self.render_mode_popover(cx);
                Some((
                    PickerKind::Mode,
                    self.popover_frame_flush(200.0, content, cx),
                ))
            }
            None => None,
        };

        // Left cluster: empty — the device/project pickers live in the
        // composer FOOTER row alongside checkout + ref.
        // Right cluster: agent+model and traits — the composer appends
        // attach + send after this element (hearth composer-actions.tsx
        // arrangement).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(4.0));
        // Model chip (brand icon + model name) beside a separate Traits chip
        // (t3code TraitsPicker arrangement): the trigger label is the joined
        // effective summary ("High · 1M · Fast", "Agent · Balance") so the
        // run's traits read without opening; it brightens only when something
        // departs from its default. No chip at all when the model has neither
        // a ladder nor options (e.g. Hermes today) — a dead trigger reads as
        // broken.
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            true,
            Some(harness_icon),
            &theme,
            cx,
        );
        let has_traits = !self.trait_ladder(cx).is_empty()
            || self
                .selected_model(cx)
                .is_some_and(|m| !m.options.is_empty());
        let traits_chip = has_traits.then(|| {
            self.trigger_chip(
                PickerKind::Traits,
                traits_label,
                traits_active,
                None,
                &theme,
                cx,
            )
        });
        // Mode chip (plan/agent/chat): the trigger label is the current mode,
        // brightening only when a non-default mode is active. Always shown —
        // every harness has a mode (Huginn's plan/agent/chat; others fall back
        // to their own default when `None`).
        let mode_label: SharedString = self
            .effective_mode(cx)
            .map(|m| SharedString::from(mode_label(m)))
            .unwrap_or_else(|| SharedString::from("Mode"));
        let mode_chip = self.trigger_chip(
            PickerKind::Mode,
            mode_label,
            self.effective_mode(cx).is_some(),
            None,
            &theme,
            cx,
        );
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(4.0))
            // End-anchored: the menu's right edge sits flush with the chip's
            // right edge (user request), same as the footer's ref popover.
            .child(attach_overlay_end(
                mode_chip,
                &mut overlay,
                PickerKind::Mode,
                "mode-popover",
                closing,
            ))
            .child(attach_overlay_end(
                model_chip,
                &mut overlay,
                PickerKind::HarnessModel,
                "model-popover",
                closing,
            ))
            .children(traits_chip.map(|chip| {
                attach_overlay_end(
                    chip,
                    &mut overlay,
                    PickerKind::Traits,
                    "traits-popover",
                    closing,
                )
            }));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            // GPUI dispatches this captured stream while the thumb is dragged,
            // including when the pointer has left the model popover.
            .on_drag_move(cx.listener(Self::on_model_scrollbar_drag_move))
            .child(left)
            .child(right)
    }
}
