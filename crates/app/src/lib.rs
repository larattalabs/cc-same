//! CC Same: keep Claude Desktop's local sessions identical across all your accounts.
//!
//! One small window and a tray icon, built with Longbridge's GPUI Kit, over the same engine as
//! the command line.

rust_i18n::i18n!("locales", fallback = "en");

pub mod assets;
pub mod hero;
pub mod i18n;
mod login;
pub mod model;
mod settings;
pub mod store;
pub mod theme;
mod tray;
pub mod update;
pub mod view;

use crate::store::Store;
use crate::tray::{Command, Tray};
use cc_same_core::{Config, Ctx, FakeDesktop, LogSink, Paths};
use gpui_kit::component::TitleBar;
use gpui_kit::{
    App, AppContext as _, Bounds, Context, Entity, Global, KeyBinding, Menu, MenuItem, QuitMode, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, px, size,
};
use std::rc::Rc;
use std::sync::Arc;

pub const APP_ID: &str = "io.github.songkeys.cc-same";
pub const APP_NAME: &str = "CC Same";

actions!(cc_same_app, [Quit, CloseWindow, CheckForUpdates]);

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
    update::finish(&ctx.paths);
    let app = gpui_kit::application().with_assets(assets::Assets).with_quit_mode(QuitMode::Explicit);
    // Clicking the Dock icon brings the window back.
    app.on_reopen(show_window);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.set_app_identity(APP_ID, APP_NAME);
        i18n::apply(&ctx.config().language);
        theme::install(cx);
        let store = Store::init(ctx.clone(), cx);
        cx.on_action(|_: &Quit, cx| quit(cx));
        cx.on_action(|_: &CheckForUpdates, cx| on_tray(Command::Update, cx));
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
        Shell::init(store, cx);
        // Without a tray icon (turned off, or a desktop that shows none) there is nothing left to
        // use once the window closes.
        cx.on_window_closed(|cx, _| {
            if !cx.windows().is_empty() {
                return;
            }
            if Shell::has_tray(cx) {
                // Nobody is looking: the moment for an update that is ready.
                Store::global(cx).update(cx, |store, cx| store.install_when_idle(cx));
            } else {
                quit(cx);
            }
        })
        .detach();
        // At login the app starts quietly in the tray; otherwise it opens its window.
        if !(login::started_hidden() && Shell::has_tray(cx)) {
            show_window(cx);
        }
    });
}

/// Bring the window forward, opening it if it was closed.
pub fn show_window(cx: &mut App) {
    if let Some(handle) = cx.windows().first().copied() {
        // A window that is handling the click that got here (a menu command) cannot be updated
        // until it is done; it is still there, so bring it forward right after.
        if handle.update(cx, |_, window, _| window.activate_window()).is_err() {
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            });
        }
        cx.activate(true);
        return;
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
        MenuItem::action(i18n::t("app.check_updates"), CheckForUpdates),
        MenuItem::separator(),
        MenuItem::action(i18n::t("app.quit"), Quit),
    ])]);
}

/// App-wide chrome that follows the settings: the interface language and the tray icon.
struct Shell {
    language: String,
    tray: Option<Tray>,
    /// The tray icon could not be added, say on a Linux desktop without StatusNotifier support.
    /// Tried again once the setting is turned off and on, not on every refresh.
    tray_failed: bool,
    _settings: Subscription,
}

impl Global for Shell {}

impl Shell {
    fn init(store: Entity<Store>, cx: &mut App) {
        let language = store.read(cx).config().language;
        let settings = cx.observe(&store, |store, cx| Shell::follow(&store, cx));
        cx.set_global(Shell { language, tray: None, tray_failed: false, _settings: settings });
        Shell::follow(&store, cx);
    }

    /// Whether a tray icon is actually showing, not just turned on.
    fn has_tray(cx: &App) -> bool {
        cx.try_global::<Shell>().is_some_and(|shell| shell.tray.is_some())
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
        let shell = cx.global::<Shell>();
        match (config.tray, shell.tray.is_some(), shell.tray_failed) {
            (true, false, false) => match Tray::install(&status, Rc::new(on_tray), cx) {
                Ok(tray) => cx.global_mut::<Shell>().tray = Some(tray),
                Err(e) => {
                    store.read(cx).ctx.log(format!("tray: {e:#}"));
                    cx.global_mut::<Shell>().tray_failed = true;
                }
            },
            (true, true, _) => {
                if let Some(tray) = &cx.global::<Shell>().tray {
                    tray.update(&status);
                }
            }
            (false, true, _) | (false, false, true) => {
                let shell = cx.global_mut::<Shell>();
                shell.tray = None;
                shell.tray_failed = false;
            }
            (true, false, true) | (false, false, false) => {}
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
        Command::Update => {
            show_window(cx);
            store.update(cx, |s, cx| s.update_or_check(cx));
        }
        // A menu item is easy to hit by mistake, and switching restarts Claude: the window asks first.
        Command::SwitchTo(account) => in_window(cx, move |view, window, cx| view.confirm_switch(account, window, cx)),
        Command::SwitchNext => in_window(cx, |view, window, cx| view.confirm_next(window, cx)),
        // The window explains first: Claude restarts signed out.
        Command::SignInAnother => in_window(cx, |view, window, cx| view.confirm_sign_in(None, window, cx)),
        Command::Quit => quit(cx),
    }
}

/// Bring the window forward, then hand its view `act` once it is up (a cycle later).
fn in_window(cx: &mut App, act: impl FnOnce(&mut view::UniApp, &mut Window, &mut Context<view::UniApp>) + 'static) {
    show_window(cx);
    cx.defer(move |cx| {
        let mut act = Some(act);
        for handle in cx.windows() {
            let _ = handle.update(cx, |root, window, cx| {
                if let Ok(root) = root.downcast::<gpui_kit::component::Root>()
                    && let Ok(view) = root.read(cx).view().clone().downcast::<view::UniApp>()
                    && let Some(act) = act.take()
                {
                    view.update(cx, |view, cx| act(view, window, cx));
                }
            });
        }
    });
}

/// Quit, putting a downloaded update in place on the way out when updates install by themselves.
fn quit(cx: &mut App) {
    Store::global(cx).update(cx, |store, _| store.install_on_quit());
    cx.quit();
}
