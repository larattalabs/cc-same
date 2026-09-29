//! The main window: a picture of your accounts and one sentence about them, the switch that keeps
//! them in step, the accounts themselves, and a note only when something needs you. Settings,
//! snapshots and files live in a sheet behind the gear.

use crate::hero;
use crate::i18n::{t, tf, tn};
use crate::model::{self, Account, Busy, Mood};
use crate::settings;
use crate::store::{Store, StoreEvent};
use crate::theme;
use cc_same_core::Surface;
use cc_same_core::report::Warning;
use cc_same_core::retention::Limit;
use cc_same_core::snapshot::Manifest;
use gpui_kit::base::{Easing, Transition, transition};
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, Context, Div, ElementId, Entity, FocusHandle, FontWeight,
    HighlightStyle, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Window, actions, div, ease_out_quint,
    linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px, rems,
};
use std::rc::Rc;
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
                Some(
                    Button::new("open-log")
                        .small()
                        .outline()
                        .label(t("notice.show_log"))
                        .on_click(move |_, _, cx| cx.open_with_system(&log))
                        .into_any_element(),
                ),
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
            notices.push(notice("notice-retention", Tone::Warning, text, action, cx));
        }
        notices
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

#[derive(Clone, Copy)]
enum Tone {
    Info,
    Warning,
    Danger,
}

/// A short note with an optional action, tinted by what it is about. It slides in once.
fn notice(id: &'static str, tone: Tone, text: String, action: Option<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (icon, color) = match tone {
        Tone::Info => (IconName::Info, theme.info),
        Tone::Warning => (IconName::TriangleAlert, theme.warning),
        Tone::Danger => (IconName::CircleAlert, theme.danger),
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
        .child(div().pt(px(2.)).child(Icon::new(icon).small().text_color(color)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2p5()
                .child(div().text_size(rems(0.923)).line_height(rems(1.3)).child(text))
                .when_some(action, |el, action| el.child(h_flex().child(action))),
        )
        .with_animation(
            ElementId::Name(format!("{id}-enter").into()),
            Animation::new(Duration::from_millis(320)).with_easing(ease_out_quint()),
            |el, t| el.opacity(t).top(px(-6. * (1. - t))),
        )
        .into_any_element()
}
