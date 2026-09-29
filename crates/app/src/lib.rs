//! CC Same: keep Claude Desktop's local sessions identical across all your accounts.
//!
//! One small window and a tray icon, built with Longbridge's GPUI Kit, over the same engine as
//! the command line.

rust_i18n::i18n!("locales", fallback = "en");

pub mod hero;
pub mod i18n;
mod login;
pub mod model;
mod settings;
pub mod store;
pub mod theme;
mod tray;
pub mod view;

use crate::store::Store;
use crate::tray::{Command, Tray};
use cc_same_core::{Config, Ctx, FakeDesktop, LogSink, Paths};
use gpui_kit::component::TitleBar;
use gpui_kit::{
    App, AppContext as _, Bounds, Entity, Global, KeyBinding, Menu, MenuItem, QuitMode, Subscription, TitlebarOptions,
    WindowBounds, WindowOptions, actions, px, size,
};
use std::rc::Rc;
use std::sync::Arc;

pub const APP_ID: &str = "io.github.songkeys.cc-same";
pub const APP_NAME: &str = "CC Same";

actions!(cc_same_app, [Quit, CloseWindow]);

/// The shared context for this process: default paths (or `CC_SAME_*` overrides).
pub fn context() -> Arc<Ctx> {
    let paths = Paths::detect();
    let config = Config::load(&paths).unwrap_or_default();
    Arc::new(Ctx::new(paths, config, FakeDesktop::from_env(), LogSink::Silent))
}

/// The window: small, centered, with GPUI Kit's title bar drawing the chrome on every platform.
pub fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions { title: Some(APP_NAME.into()), ..TitleBar::title_bar_options() }),
        app_id: Some(APP_ID.into()),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(420.), px(600.)), cx))),
        window_min_size: Some(size(px(380.), px(520.))),
        ..TitleBar::window_options()
    }
}

pub fn run() {
    let ctx = context();
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets).with_quit_mode(QuitMode::Explicit);
    // Clicking the Dock icon brings the window back.
    app.on_reopen(show_window);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.set_app_identity(APP_ID, APP_NAME);
        i18n::apply(&ctx.config().language);
        theme::install(cx);
        let store = Store::init(ctx.clone(), cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, Some("UniApp")),
            KeyBinding::new("ctrl-w", CloseWindow, Some("UniApp")),
            KeyBinding::new("cmd-r", view::Refresh, Some("UniApp")),
            KeyBinding::new("ctrl-r", view::Refresh, Some("UniApp")),
            KeyBinding::new("cmd-,", view::OpenSettings, Some("UniApp")),
            KeyBinding::new("ctrl-,", view::OpenSettings, Some("UniApp")),
        ]);
        set_menus(cx);
        Shell::init(store.clone(), cx);
        // Without a tray icon there is nothing left to use once the window closes.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() && !Store::global(cx).read(cx).config().tray {
                cx.quit();
            }
        })
        .detach();
        // At login the app starts quietly in the tray; otherwise it opens its window.
        if !(login::started_hidden() && store.read(cx).config().tray) {
            show_window(cx);
        }
    });
}

/// Bring the window forward, opening it if it was closed.
pub fn show_window(cx: &mut App) {
    for handle in cx.windows() {
        if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
            cx.activate(true);
            return;
        }
    }
    let store = Store::global(cx);
    let options = window_options(cx);
    match gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| view::UniApp::new(store.clone(), window, cx))) {
        Ok(_) => cx.activate(true),
        Err(e) => eprintln!("could not open the {APP_NAME} window: {e:#}"),
    }
}

fn set_menus(cx: &mut App) {
    cx.set_menus([Menu::new(APP_NAME).items([
        MenuItem::action(i18n::t("app.settings"), view::OpenSettings),
        MenuItem::separator(),
        MenuItem::action(i18n::t("app.quit"), Quit),
    ])]);
}

/// App-wide chrome that follows the settings: the interface language and the tray icon.
struct Shell {
    language: String,
    tray: Option<Tray>,
    _settings: Subscription,
}

impl Global for Shell {}

impl Shell {
    fn init(store: Entity<Store>, cx: &mut App) {
        let language = store.read(cx).config().language;
        let settings = cx.observe(&store, |store, cx| Shell::follow(&store, cx));
        cx.set_global(Shell { language, tray: None, _settings: settings });
        Shell::follow(&store, cx);
    }

    fn follow(store: &Entity<Store>, cx: &mut App) {
        let config = store.read(cx).config();
        if cx.global::<Shell>().language != config.language {
            i18n::apply(&config.language);
            set_menus(cx);
            cx.refresh_windows();
            cx.global_mut::<Shell>().language = config.language.clone();
        }
        let status = tray::Status::of(store.read(cx));
        let has_tray = cx.global::<Shell>().tray.is_some();
        match (config.tray, has_tray) {
            (true, false) => match Tray::install(&status, Rc::new(on_tray), cx) {
                Ok(tray) => cx.global_mut::<Shell>().tray = Some(tray),
                Err(e) => store.read(cx).ctx.log(format!("tray: {e:#}")),
            },
            (false, true) => cx.global_mut::<Shell>().tray = None,
            (true, true) => {
                if let Some(tray) = &cx.global::<Shell>().tray {
                    tray.update(&status);
                }
            }
            (false, false) => {}
        }
    }
}

fn on_tray(command: Command, cx: &mut App) {
    let store = Store::global(cx);
    match command {
        Command::Open => show_window(cx),
        Command::SyncNow => store.update(cx, |s, cx| s.sync_now(cx)),
        Command::ToggleBackground => {
            let on = store.read(cx).background_on();
            store.update(cx, |s, cx| s.set_background(!on, cx));
        }
        Command::Settings => {
            show_window(cx);
            for handle in cx.windows() {
                let _ = handle.update(cx, |root, window, cx| {
                    if let Ok(root) = root.downcast::<gpui_kit::component::Root>()
                        && let Ok(view) = root.read(cx).view().clone().downcast::<view::UniApp>()
                    {
                        view.update(cx, |view, cx| view.open_settings(window, cx));
                    }
                });
            }
        }
        Command::Quit => cx.quit(),
    }
}
