//! The main window: a picture of your accounts and one sentence about them, the switch that keeps
//! them in step, the accounts themselves, and a note only when something needs you. Settings,
//! snapshots and files live in a sheet behind the gear.

use crate::hero;
use crate::i18n::{t, tf, tn};
use crate::model::{self, Account, Busy, Mood};
use crate::settings;
use crate::store::{Store, StoreEvent, UpdatePhase};
use crate::theme;
use crate::update::{self, Release};
use cc_same_core::Surface;
use cc_same_core::report::Warning;
use cc_same_core::retention::Limit;
use cc_same_core::snapshot::Manifest;
use gpui_kit::base::{Easing, Transition, transition};
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle,
    FontWeight, HighlightStyle, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Window, actions, div, ease_out_quint,
    linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px, rems,
};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

actions!(cc_same, [Refresh, SyncNow, OpenSettings]);

pub struct UniApp {
    store: Entity<Store>,
    /// The account under the pointer, in the list or in the picture.
    hovered: Option<SharedString>,
    /// False until the first frame with data has been drawn, so the rings fly in and the
    /// numbers count up.
    gathered: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl UniApp {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> UniApp {
        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&store, window, |_, _, event: &StoreEvent, window, cx| {
                let StoreEvent::Done(result) = event;
                match result {
                    Ok(message) if message.is_empty() => {}
                    Ok(message) => window.push_notification(Notification::success(message.clone()), cx),
                    Err(error) => window.push_notification(Notification::error(error.clone()).autohide(false), cx),
                }
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        UniApp { store, hovered: None, gathered: false, focus, _subscriptions: subscriptions }
    }

    /// A still picture (screenshots): everything already in place.
    pub fn still(store: Entity<Store>, cx: &mut Context<Self>) -> UniApp {
        UniApp { store, hovered: None, gathered: true, focus: cx.focus_handle(), _subscriptions: Vec::new() }
    }

    pub fn store(&self) -> &Entity<Store> {
        &self.store
    }

    // ------------------------------------------------------------------ actions

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.sync_now(cx));
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        settings::open(self.store.clone(), window, cx);
    }

    pub fn confirm_restore(&mut self, snapshot: &Manifest, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let id = snapshot.id.clone();
        let when = model::snapshot_when(&snapshot.id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (store, id) = (store.clone(), id.clone());
            alert
                .title(tf("restore.title", &[("when", &when)]))
                .description(t("restore.body"))
                .confirm()
                .ok_text(t("restore.ok"))
                .cancel_text(t("dialog.cancel"))
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    store.update(cx, |store, cx| store.restore(id, cx));
                    true
                })
        });
    }

    fn confirm_fix_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            alert
                .title(t("fix.title"))
                .description(t("fix.body"))
                .confirm()
                .ok_text(t("fix.ok"))
                .cancel_text(t("dialog.cancel"))
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, cx| store.fix_links(cx));
                    true
                })
        });
    }

    fn set_hovered(&mut self, key: Option<SharedString>, cx: &mut Context<Self>) {
        if self.hovered != key {
            self.hovered = key;
            cx.notify();
        }
    }

    // ------------------------------------------------------------------ rendering

    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        TitleBar::new()
            .bg(gpui_kit::transparent_black())
            .border_color(gpui_kit::transparent_black())
            .child(
                h_flex().w_full().justify_end().pr_2().child(
                    Button::new("open-settings")
                        .ghost()
                        .small()
                        .icon(IconName::Settings2)
                        .tooltip(t("settings.title"))
                        .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_headline(
        &mut self,
        mood: Mood,
        accounts: &[Account],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let on_hover: hero::OnHover = Rc::new(move |key, _, cx| {
            let _ = view.update(cx, |this, cx| this.set_hovered(key, cx));
        });
        let gathered = self.gathered || mood == Mood::Loading;
        let picture = hero::rings(accounts, mood, self.hovered.as_deref(), gathered, on_hover, window, cx);

        // The count rolls up to its value when the window opens, and on to a new one.
        let counted = |n: usize, window: &mut Window, cx: &mut Context<Self>| -> usize {
            let target = if gathered { n as f32 } else { 0. };
            let value = transition(
                "headline-count",
                target,
                Transition::new(Duration::from_millis(900)).easing(Easing::EaseOut),
                window,
                cx,
            );
            value.round() as usize
        };
        let shown = match mood {
            Mood::InSync { sessions, accounts } => Mood::InSync { sessions: counted(sessions, window, cx), accounts },
            Mood::Pending { sessions, other } if sessions > 0 => {
                Mood::Pending { sessions: counted(sessions, window, cx), other }
            }
            other => other,
        };

        let (overview, busy) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy)
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let synced = overview
            .as_ref()
            .and_then(|o| o.last_sync.as_ref())
            .filter(|_| matches!(mood, Mood::InSync { .. } | Mood::Waiting(_)))
            .map(|ls| tf("synced_ago", &[("ago", &crate::i18n::ago(ls.at))]));
        let running = overview.as_ref().is_some_and(|o| o.app.running);
        let action: Option<AnyElement> = match mood {
            Mood::Pending { .. } => Some(
                Button::new("sync-now")
                    .primary()
                    .label(t("action.sync_now"))
                    .icon(IconName::RefreshCw)
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx)))
                    .into_any_element(),
            ),
            Mood::Linked(_) => Some(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("fix-links")
                            .primary()
                            .label(t("action.fix_folders"))
                            .disabled(running || busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_fix_links(window, cx))),
                    )
                    .when(running, |el| {
                        el.child(div().text_size(rems(0.923)).text_color(muted).child(t("hint.quit_first")))
                    })
                    .into_any_element(),
            ),
            _ => None,
        };

        let detail = shown.detail();
        let words = v_flex()
            .relative()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_size(rems(1.54))
                    .line_height(rems(1.9))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_center()
                    .child(shown.title()),
            )
            .when(!detail.is_empty(), |el| {
                el.child(div().text_center().text_color(muted).child(emphasize_numbers(detail, foreground)))
            })
            // A new sentence rises into place; a number that changes does not replay it.
            .with_animation(
                ElementId::Name(format!("headline-{}", mood_kind(mood)).into()),
                Animation::new(Duration::from_millis(360)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t).top(px(5. * (1. - t))),
            );

        v_flex()
            .items_center()
            .px_8()
            .pb_6()
            .child(picture)
            .child(words)
            .when_some(synced, |el, synced| {
                el.child(div().mt_1p5().text_size(rems(0.923)).text_color(muted.opacity(0.8)).child(synced))
            })
            .when_some(action, |el, action| el.child(div().mt_4().child(action)))
    }

    fn render_background(&self, cx: &mut Context<Self>) -> AnyElement {
        let (overview, busy, switched_on_at) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy, store.switched_on_at)
        };
        let theme = cx.theme();
        let st = overview.as_ref().map(|o| o.service.clone()).unwrap_or_default();
        let alive = overview.as_deref().is_some_and(model::agent_alive);
        // Just switched on: the agent is starting and has not checked in yet.
        let starting = switched_on_at.is_some_and(|t| t.elapsed() < Duration::from_secs(90));
        let (line, tone): (String, Hsla) = if st.installed && alive {
            let checked =
                overview.as_ref().and_then(|o| o.heartbeat.as_ref()).map(|h| crate::i18n::ago(h.heartbeat_at));
            (tf("background.on", &[("ago", &checked.unwrap_or_else(|| t("time.just_now")))]), theme.muted_foreground)
        } else if st.installed && starting {
            (t("background.starting"), theme.muted_foreground)
        } else if st.installed {
            let mut line = t("background.stalled");
            if cfg!(target_os = "macos") {
                line = format!("{line} {}", t("background.stalled_mac"));
            }
            (line, theme.warning)
        } else if st.legacy {
            (t("background.legacy"), theme.warning)
        } else {
            (t("background.off"), theme.muted_foreground)
        };
        let switching = busy == Some(Busy::Service);
        let store = self.store.clone();
        surface(cx)
            .child(
                h_flex()
                    .gap_4()
                    .px_4()
                    .py_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().font_weight(FontWeight::MEDIUM).child(t("background.title")))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_size(rems(0.923))
                                    .text_color(tone)
                                    .when(st.installed && alive, |el| el.child(dot(theme.success)))
                                    .child(div().min_w_0().child(line)),
                            ),
                    )
                    .child(if switching {
                        Spinner::new().small().into_any_element()
                    } else {
                        Switch::new("background")
                            .checked(st.installed)
                            .disabled(busy.is_some() || overview.is_none())
                            .on_change(move |on: &bool, _, cx| {
                                let on = *on;
                                store.update(cx, |store, cx| store.set_background(on, cx));
                            })
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_accounts(&self, accounts: &[Account], cx: &mut Context<Self>) -> AnyElement {
        let loading = self.store.read(cx).overview.is_none();
        let cowork = self.store.read(cx).config().syncs(Surface::Cowork);
        let theme = cx.theme();
        let rows: Vec<AnyElement> = if loading {
            (0..3).map(|i| skeleton_row(i, cx)).collect()
        } else {
            accounts
                .iter()
                .map(|a| {
                    let key: SharedString = a.key.clone().into();
                    let hovered = self.hovered.as_ref() == Some(&key);
                    let hover_key = key.clone();
                    h_flex()
                        .id(ElementId::Name(format!("account-{}", a.key).into()))
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .when(hovered, |el| el.bg(theme.list_hover))
                        .on_hover(cx.listener(move |this, inside: &bool, _, cx| {
                            let key = inside.then(|| hover_key.clone());
                            if key.is_some() || this.hovered.as_ref() == Some(&hover_key) {
                                this.set_hovered(key, cx);
                            }
                        }))
                        .when(a.excluded, |el| el.opacity(0.55))
                        .child(Avatar::new().name(a.initials()).small())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().font_weight(FontWeight::MEDIUM).child(a.name.clone()))
                                .child(
                                    div()
                                        .when(!a.broken, |el| el.truncate())
                                        .text_size(rems(0.923))
                                        .text_color(if a.broken { theme.danger } else { theme.muted_foreground })
                                        .child(a.detail()),
                                ),
                        )
                        .when(a.open, |el| {
                            el.child(
                                Tag::secondary()
                                    .small()
                                    .rounded_full()
                                    .child(h_flex().gap_1p5().child(dot(theme.primary)).child(t("account.open"))),
                            )
                        })
                        .into_any_element()
                })
                .collect()
        };
        let empty = rows.is_empty();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .px_1()
                    .justify_between()
                    .child(section_label(t("accounts.title"), cx))
                    .when(cowork, |el| el.child(section_label(t("accounts.both"), cx))),
            )
            .child(surface(cx).p_1().gap_px().children(rows).when(empty, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_3()
                        .text_size(rems(0.923))
                        .text_color(cx.theme().muted_foreground)
                        .child(t("accounts.empty")),
                )
            }))
            .into_any_element()
    }

    fn render_notices(&self, accounts: &[Account], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        let (overview, busy, log) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy, store.ctx.paths.log_file.clone())
        };
        let Some(ov) = overview else { return notices };
        if let Some(hint) = ov.restart_hint() {
            let open =
                accounts.iter().find(|a| a.open).map(|a| a.name.clone()).unwrap_or_else(|| t("notice.open_account"));
            notices.push(notice(
                "notice-restart",
                Tone::Info,
                tn("notice.restart", hint.sessions as usize, &[("account", &open)]),
                Vec::new(),
                None,
                cx,
            ));
        }
        if let Some(ls) = &ov.last_sync
            && ls.errors > 0
        {
            notices.push(notice(
                "notice-errors",
                Tone::Danger,
                tn("notice.errors", ls.errors as usize, &[]),
                vec![
                    Button::new("open-log")
                        .small()
                        .outline()
                        .label(t("notice.show_log"))
                        .on_click(move |_, _, cx| cx.open_with_system(&log))
                        .into_any_element(),
                ],
                None,
                cx,
            ));
        }
        // Only when Claude Code will actually delete the transcripts behind Desktop sessions.
        if let Some((days, limited_by)) = ov.warnings.iter().find_map(|w| match w {
            Warning::ShortRetention { days, limited_by, .. } => Some((*days, *limited_by)),
            _ => None,
        }) {
            let mut text = tn("notice.retention", days.round() as usize, &[]);
            match limited_by {
                Limit::Organization => text = format!("{text} {}", t("notice.retention_org")),
                Limit::OlderClaudeCode => text = format!("{text} {}", t("notice.retention_old")),
                Limit::DesktopSetting => {}
            }
            let action = (limited_by != Limit::Organization).then(|| {
                let store = self.store.clone();
                let label = match limited_by {
                    Limit::OlderClaudeCode => t("notice.keep_10_years"),
                    _ => t("notice.keep_them"),
                };
                Button::new("keep-transcripts")
                    .small()
                    .outline()
                    .label(label)
                    .disabled(busy.is_some())
                    .on_click(move |_, _, cx| store.update(cx, |store, cx| store.keep_transcripts(cx)))
                    .into_any_element()
            });
            notices.push(notice("notice-retention", Tone::Warning, text, action.into_iter().collect(), None, cx));
        }
        notices.extend(self.render_update_notice(cx));
        notices
    }

    /// A new version: available, on its way, ready, or just installed.
    fn render_update_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (updates, busy) = {
            let store = self.store.read(cx);
            (store.updates.clone(), store.busy)
        };
        let store = self.store.clone();
        let update_now = |id: &'static str, label: String, enabled: bool| -> AnyElement {
            let store = store.clone();
            Button::new(id)
                .small()
                .primary()
                .label(label)
                .disabled(!enabled)
                .on_click(move |_, _, cx| store.update(cx, |store, cx| store.update_or_check(cx)))
                .into_any_element()
        };
        let Some(release) = updates.available.clone() else {
            // Just updated: what changed since the version before.
            let from = updates.updated_from.clone()?;
            let notes = update::changes_since(semver::Version::parse(&from).ok().as_ref());
            let title = tf("update.notes_title", &[("version", &update::VERSION)]);
            let dismiss = {
                let store = store.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| store.update(cx, |s, cx| s.dismiss_updated(cx))
            };
            let read = Button::new("updated-notes").small().outline().label(t("update.whats_new")).on_click({
                let dismiss = dismiss.clone();
                move |event, window, cx| {
                    open_notes(title.clone(), notes.clone(), window, cx);
                    dismiss(event, window, cx);
                }
            });
            let close = Button::new("updated-close").ghost().xsmall().icon(IconName::Close).on_click(dismiss);
            let text = tf("update.updated", &[("version", &update::VERSION)]);
            return Some(notice(
                "notice-updated",
                Tone::New,
                text,
                vec![read.into_any_element()],
                Some(close.into_any_element()),
                cx,
            ));
        };
        let version = release.version.to_string();
        let whats_new = release_notes_button(&release);
        let (tone, text, actions) = match &updates.phase {
            UpdatePhase::Downloading(_) => {
                let percent = updates.progress().unwrap_or(0.) * 100.;
                let bar = div().w_full().pt_0p5().child(Progress::new("update-progress").value(percent));
                (Tone::Update, tf("update.downloading", &[("version", &version)]), vec![bar.into_any_element()])
            }
            UpdatePhase::Installing => (Tone::Update, tf("update.installing", &[("version", &version)]), Vec::new()),
            UpdatePhase::Ready(_) => (
                Tone::Update,
                tf("update.ready", &[("version", &version)]),
                vec![update_now("update-restart", t("update.restart"), busy.is_none()), whats_new],
            ),
            UpdatePhase::Idle | UpdatePhase::Checking => match &updates.problem {
                Some((problem, _)) => {
                    let page = release.page.clone();
                    let download = Button::new("update-download")
                        .small()
                        .outline()
                        .icon(IconName::ExternalLink)
                        .label(t("update.download"))
                        .on_click(move |_, _, cx| cx.open_url(&page));
                    let retry = Button::new("update-retry").small().outline().label(t("update.retry")).on_click({
                        let store = store.clone();
                        move |_, _, cx| store.update(cx, |store, cx| store.update_or_check(cx))
                    });
                    (
                        Tone::Warning,
                        t(problem.message_key()),
                        vec![retry.disabled(updates.working()).into_any_element(), download.into_any_element()],
                    )
                }
                None => (
                    Tone::Update,
                    tf("update.available", &[("version", &version)]),
                    vec![update_now("update-now", t("update.install"), !updates.working()), whats_new],
                ),
            },
        };
        Some(notice("notice-update", tone, text, actions, None, cx))
    }
}

impl Render for UniApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme_choice, mood, accounts, loaded) = {
            let store = self.store.read(cx);
            (store.theme(), store.mood(), store.accounts(), store.overview.is_some())
        };
        theme::follow(theme_choice, window, cx);
        if loaded && !self.gathered {
            cx.on_next_frame(window, |this, _, cx| {
                this.gathered = true;
                cx.notify();
            });
        }
        let headline = self.render_headline(mood, &accounts, window, cx);
        let background = self.render_background(cx);
        let list = self.render_accounts(&accounts, cx);
        let notices = self.render_notices(&accounts, cx);
        let (background_color, text_color, wash) = {
            let theme = cx.theme();
            (theme.background, theme.foreground, theme::wash(cx))
        };
        v_flex()
            .relative()
            .key_context("UniApp")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.store.update(cx, |s, cx| s.refresh(cx))))
            .on_action(cx.listener(|this, _: &SyncNow, _, cx| this.sync_now(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|_, _: &crate::CloseWindow, window, _| window.remove_window()))
            .size_full()
            .bg(background_color)
            .text_color(text_color)
            // A warm light falls on the top of the window.
            .child(div().absolute().top_0().left_0().right_0().h(px(300.)).bg(linear_gradient(
                180.,
                linear_color_stop(wash, 0.),
                linear_color_stop(wash.opacity(0.), 1.),
            )))
            .child(self.render_title_bar(cx))
            .child(
                div().id("content").flex_1().min_h_0().overflow_y_scrollbar().child(
                    v_flex()
                        .pb_6()
                        .child(headline)
                        .child(v_flex().px_5().gap_5().children(notices).child(background).child(list)),
                ),
            )
    }
}

/// Which headline is showing, ignoring its numbers.
fn mood_kind(mood: Mood) -> &'static str {
    match mood {
        Mood::Loading => "loading",
        Mood::Missing => "missing",
        Mood::Single => "single",
        Mood::Linked(_) => "linked",
        Mood::Syncing { .. } => "syncing",
        Mood::Pending { .. } => "pending",
        Mood::Waiting(_) => "waiting",
        Mood::InSync { .. } => "in-sync",
    }
}

/// A placeholder row while the accounts load.
fn skeleton_row(i: usize, cx: &App) -> AnyElement {
    h_flex()
        .gap_3()
        .px_3()
        .py_2()
        .child(Skeleton::new().size(px(24.)).rounded_full())
        .child(
            v_flex()
                .flex_1()
                .gap_1p5()
                .child(Skeleton::new().h(rems(0.8)).w(rems([9., 11., 7.5][i % 3])).rounded(cx.theme().radius))
                .child(Skeleton::new().secondary().h(rems(0.7)).w(rems(5.5)).rounded(cx.theme().radius)),
        )
        .into_any_element()
}

/// The raised surface every group of controls sits on.
pub(crate) fn surface(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .bg(theme.group_box)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius_lg)
        .shadow(theme::card_shadow(cx))
}

fn section_label(text: String, cx: &App) -> impl IntoElement {
    div().text_size(rems(0.846)).font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(text)
}

fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(6.)).flex_none().rounded_full().bg(color)
}

/// Numbers in a sentence carry the news, so they get the full text color.
fn emphasize_numbers(text: String, color: Hsla) -> StyledText {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_ascii_digit(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let style =
                    HighlightStyle { color: Some(color), font_weight: Some(FontWeight::MEDIUM), ..Default::default() };
                runs.push((s..i, style));
                start = None;
            }
            _ => {}
        }
    }
    StyledText::new(text).with_highlights(runs)
}

// Two Lucide icons (ISC license) that GPUI Kit's default icon set leaves out.
const CIRCLE_ARROW_UP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="m16 12-4-4-4 4"/><path d="M12 16V8"/></svg>"#;
const SPARKLES: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z"/><path d="M20 2v4"/><path d="M22 4h-4"/><circle cx="4" cy="20" r="2"/></svg>"#;

#[derive(Clone, Copy)]
enum Tone {
    Info,
    Warning,
    Danger,
    /// A new version is available or on its way.
    Update,
    /// This version is new.
    New,
}

/// A short note with its actions, tinted by what it is about, and a close button when it can be
/// put away. It slides in once.
fn notice(
    id: &'static str,
    tone: Tone,
    text: String,
    actions: Vec<AnyElement>,
    close: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (icon, color) = match tone {
        Tone::Info => (Icon::new(IconName::Info), theme.info),
        Tone::Warning => (Icon::new(IconName::TriangleAlert), theme.warning),
        Tone::Danger => (Icon::new(IconName::CircleAlert), theme.danger),
        Tone::Update => (Icon::default().data(CIRCLE_ARROW_UP), theme.primary),
        Tone::New => (Icon::default().data(SPARKLES), theme.primary),
    };
    h_flex()
        .id(id)
        .relative()
        .items_start()
        .gap_3()
        .px_3p5()
        .py_3()
        .rounded(theme.radius_lg)
        .bg(color.opacity(if theme.is_dark() { 0.09 } else { 0.06 }))
        .border_1()
        .border_color(color.opacity(0.22))
        .child(div().pt(px(2.)).child(icon.small().text_color(color)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2p5()
                .child(div().text_size(rems(0.923)).line_height(rems(1.3)).child(text))
                .when(!actions.is_empty(), |el| el.child(h_flex().gap_2().children(actions))),
        )
        .when_some(close, |el, close| el.child(div().mt(px(-3.)).mr(px(-6.)).child(close)))
        .with_animation(
            ElementId::Name(format!("{id}-enter").into()),
            Animation::new(Duration::from_millis(320)).with_easing(ease_out_quint()),
            |el, t| el.opacity(t).top(px(-6. * (1. - t))),
        )
        .into_any_element()
}

/// "What's new" for a release: its notes from GitHub.
fn release_notes_button(release: &Release) -> AnyElement {
    let title = tf("update.notes_title", &[("version", &release.version)]);
    let notes = release.notes.clone();
    Button::new("update-notes")
        .small()
        .ghost()
        .label(t("update.whats_new"))
        .on_click(move |_, window, cx| open_notes(title.clone(), notes.clone(), window, cx))
        .into_any_element()
}

/// Release notes, in Markdown, in a dialog.
pub fn open_notes(title: String, notes: String, window: &mut Window, cx: &mut App) {
    let notes: SharedString = if notes.trim().is_empty() { t("update.no_notes").into() } else { notes.into() };
    // Headings stay below the dialog's title: a version, then its sections.
    let style = TextViewStyle {
        paragraph_gap: rems(0.75),
        heading_font_size: Some(Arc::new(|level, _| px(if level <= 2 { 15. } else { 13. }))),
        ..TextViewStyle::default()
    };
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(380.)).child(
            TextView::markdown("release-notes", notes.clone())
                .style(style.clone())
                .text_size(rems(0.923))
                .selectable(true),
        )
    });
}
