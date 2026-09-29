//! CC Same's look: a warm light and dark theme for GPUI Kit (`assets/theme.json`), and the
//! few product colors the rings in the headline need, derived from the theme's semantic tokens.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, ThemeSet};
use gpui_kit::{Anchor, App, BoxShadow, Hsla, Window, point, px};
use std::rc::Rc;

const THEMES: &str = include_str!("../assets/theme.json");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> String {
        crate::i18n::t(match self {
            ThemeChoice::System => "theme.system",
            ThemeChoice::Light => "theme.light",
            ThemeChoice::Dark => "theme.dark",
        })
    }

    /// The `appearance` value in `config.json`.
    pub fn from_config(value: &str) -> ThemeChoice {
        match value {
            "light" => ThemeChoice::Light,
            "dark" => ThemeChoice::Dark,
            _ => ThemeChoice::System,
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }

    fn mode(self, window: &Window) -> ThemeMode {
        match self {
            ThemeChoice::System => window.appearance().into(),
            ThemeChoice::Light => ThemeMode::Light,
            ThemeChoice::Dark => ThemeMode::Dark,
        }
    }
}

/// Registers the CC Same themes as GPUI Kit's light and dark themes and loads the one that
/// matches the system.
pub fn install(cx: &mut App) {
    let set: ThemeSet = serde_json::from_str(THEMES).expect("the bundled theme is valid");
    let pick = |dark: bool| {
        let config = set.themes.iter().find(|t| t.mode.is_dark() == dark).cloned();
        Rc::new(config.expect("the bundled theme has a light and a dark variant"))
    };
    let (light, dark) = (pick(false), pick(true));
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
        theme.notification.placement = Anchor::BottomCenter;
        theme.notification.width = px(340.);
    });
    let mode: ThemeMode = cx.window_appearance().into();
    Theme::change(mode, None, cx);
}

/// Follows the chosen appearance; called on every render, so a system switch is picked up.
pub fn follow(choice: ThemeChoice, window: &mut Window, cx: &mut App) {
    let mode = choice.mode(window);
    if cx.theme().mode != mode {
        Theme::change(mode, Some(window), cx);
    }
}

/// Colors for the rings in the headline. Everything comes from the theme's semantic tokens, so
/// a custom theme repaints them too.
#[derive(Clone, Copy, Debug)]
pub struct RingColors {
    /// The outline of a ring.
    pub stroke: Hsla,
    /// The outline of a ring the pointer is over.
    pub stroke_hover: Hsla,
    /// The translucent fill; overlaps deepen where rings share sessions.
    pub fill: Hsla,
    /// The account Claude has open.
    pub open: Hsla,
    /// Initials inside a ring.
    pub label: Hsla,
    pub danger: Hsla,
    /// The faint dot grid behind the rings.
    pub grid: Hsla,
}

impl RingColors {
    pub fn new(cx: &App) -> RingColors {
        let theme = cx.theme();
        let dark = theme.is_dark();
        RingColors {
            stroke: theme.foreground.opacity(if dark { 0.24 } else { 0.18 }),
            stroke_hover: theme.foreground.opacity(if dark { 0.55 } else { 0.45 }),
            fill: theme.primary.opacity(if dark { 0.15 } else { 0.18 }),
            open: theme.primary.opacity(if dark { 0.8 } else { 0.7 }),
            label: theme.muted_foreground,
            danger: theme.danger,
            grid: theme.foreground.opacity(if dark { 0.22 } else { 0.18 }),
        }
    }
}

/// The warm light at the top of the window.
pub fn wash(cx: &App) -> Hsla {
    let theme = cx.theme();
    theme.primary.opacity(if theme.is_dark() { 0.09 } else { 0.065 })
}

/// The soft lift under a card: a contact shadow and a wide, faint one. Dark surfaces get
/// their separation from the border alone.
pub fn card_shadow(cx: &App) -> Vec<BoxShadow> {
    let theme = cx.theme();
    if theme.is_dark() {
        return Vec::new();
    }
    let ink = theme.foreground;
    vec![
        BoxShadow {
            color: ink.opacity(0.035),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: false,
        },
        BoxShadow {
            color: ink.opacity(0.04),
            offset: point(px(0.), px(6.)),
            blur_radius: px(18.),
            spread_radius: px(-4.),
            inset: false,
        },
    ]
}
