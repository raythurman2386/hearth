//! The space and device pickers: their rows, the two popovers and the picks that
//! re-key the draft.

use super::*;

impl Pickers {
    // ---- the space picker (new-session canvas) ----

    /// The picker's project rows: scoped to the canvas's device — the device
    /// switcher narrows the list, projects on other devices don't show
    /// (pick the device first, then its project). Unscoped only while the
    /// device is still unknown (pre-probe boot).
    pub(super) fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        state
            .spaces_sorted()
            .into_iter()
            .filter(|s| match device.as_deref() {
                Some(d) => s.device_id == d,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// Re-home the canvas onto another project. The state observer does the
    /// heavy lifting: branch draft, ref cache, and the per-device
    /// harness/model catalogs all invalidate on the project change.
    pub(super) fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        self.remember_target(cx);
        self.close(cx);
    }

    pub(super) fn pick_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_device(device_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the device/project picks — the "last selected" defaults the
    /// next boot's canvas restores.
    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.device = state
                .selected_device
                .clone()
                .or_else(|| state.local_device_id.clone());
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    /// Devices in picker order: this device first, then by name.
    pub(super) fn device_rows(&self, cx: &App) -> Vec<hearth_proto::Device> {
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let mut devices: Vec<hearth_proto::Device> = state.devices.clone();
        devices.sort_by_key(|d| {
            (
                local.as_deref() != Some(d.id.as_str()),
                d.name.to_lowercase(),
                d.id.clone(),
            )
        });
        devices
    }

    /// The device popover: search + one row per device (name, muted "offline"
    /// tag, check on the canvas's effective device).
    pub(super) fn render_device_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows = self.filtered_device_rows(cx);
        let (effective, local, online): (Option<String>, Option<String>, Vec<bool>) = {
            let state = self.state.read(cx);
            (
                state.effective_device_id(),
                state.local_device_id.clone(),
                rows.iter()
                    .map(|d| state.device_online(&d.id, now))
                    .collect(),
            )
        };
        let active = self.active;
        let body: AnyElement =
            if rows.is_empty() {
                div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No devices match."))
                    .into_any_element()
            } else {
                div()
                    .id("device-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .children(rows.into_iter().zip(online).enumerate().map(
                        |(ix, (device, online))| {
                            let is_local = local.as_deref() == Some(device.id.as_str());
                            let label: SharedString = device.name.clone().into();
                            let is_selected = effective.as_deref() == Some(device.id.as_str());
                            let pick_id = device.id.clone();
                            popover::menu_row_nav(
                                &theme,
                                is_selected,
                                ix == active,
                                format!("device-row-{ix}"),
                            )
                            .id(("device-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_device(pick_id.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            // The local device wears a muted right-aligned "You"
                            // instead of a "(this device)" suffix in the name.
                            .when(is_local, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from("You")),
                                )
                            })
                            // Disconnected glyph, not the word (user request).
                            .when(!online, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::WIFI_OFF)
                                        .size(px(12.0))
                                        .flex_none()
                                        .text_color(theme.warning.opacity(0.8)),
                                )
                            })
                        },
                    ))
                    .into_any_element()
            };
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .into_any_element()
    }

    /// The project popover: search + one row per project on the picked device
    /// (check on the current pick), then a "New project…" action row. Rows
    /// are device-scoped, so no per-row `@ device` tag — the device chip next
    /// door names the host.
    pub(super) fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        let active = self.active;
        let body: AnyElement = if rows.is_empty() {
            // Distinguish "the filter ate everything" from "this device has
            // no projects yet" — the scoped list makes the latter common.
            let empty: &str = if self.search.read(cx).text().is_empty() {
                "No projects on this device."
            } else {
                "No projects match."
            };
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(empty.to_string()))
                .into_any_element()
        } else {
            div()
                .id("space-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(ix, space)| {
                    let label: SharedString = space.display_name().to_string().into();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let pick_id = space.id.clone();
                    popover::menu_row_nav(
                        &theme,
                        is_selected,
                        ix == active,
                        format!("space-row-{ix}"),
                    )
                    .id(("space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_space(pick_id.clone(), cx);
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                }))
                .into_any_element()
        };
        // Action row under a hairline: mint a project.
        let new_project = popover::menu_row_nav(&theme, false, false, "project-new".to_string())
            .id("project-new")
            .on_click(cx.listener(|this, _, window, cx| {
                this.close(cx);
                window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
            }))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from("New project…")),
            );
        div()
            .flex()
            .flex_col()
            // Same 2px rhythm as the list's own row gap — the action rows
            // sat flush while list rows breathed (user report).
            .gap(px(2.0))
            .child(self.search_box(&theme))
            .child(body)
            .child(
                // Full-bleed through the card's 4px inset — a divider
                // stopping short of the edges read as a mistake.
                div()
                    .my(px(2.0))
                    .mx(px(-4.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.border.opacity(0.6)),
            )
            .child(new_project)
            .into_any_element()
    }
}
