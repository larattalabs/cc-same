//! The menu bar (macOS) and notification area (Windows, Linux) icon: the headline at a glance,
//! Sync now, the background switch, and the way back to the window.

use crate::i18n::t;
use crate::model::Busy;
use crate::store::Store;
use gpui_kit::App;
use std::rc::Rc;

/// What a click in the tray asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    SyncNow,
    ToggleBackground,
    Settings,
    Quit,
}

pub type OnCommand = Rc<dyn Fn(Command, &mut App)>;

/// Everything the tray shows, in the interface language.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub title: String,
    pub detail: String,
    pub can_sync: bool,
    pub background: bool,
    pub background_enabled: bool,
    pub attention: bool,
    pub sync_now: String,
    pub background_label: String,
    pub open: String,
    pub settings: String,
    pub quit: String,
}

impl Status {
    pub fn of(store: &Store) -> Status {
        let mood = store.mood();
        let idle = store.busy.is_none();
        Status {
            title: mood.title(),
            detail: mood.detail(),
            can_sync: idle && store.overview.is_some(),
            background: store.background_on(),
            background_enabled: idle && store.busy != Some(Busy::Service) && store.overview.is_some(),
            attention: mood.needs_attention(),
            sync_now: t("action.sync_now"),
            background_label: t("background.title"),
            open: t("app.open"),
            settings: t("app.settings"),
            quit: t("app.quit"),
        }
    }

    fn tooltip(&self) -> String {
        format!("{} — {}", crate::APP_NAME, self.title)
    }
}

/// The icon; dropping it removes it.
pub struct Tray(platform::Tray);

impl Tray {
    pub fn install(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
        platform::Tray::new(status, on_command, cx).map(Tray)
    }

    pub fn update(&self, status: &Status) {
        self.0.update(status);
    }
}

fn decode(png: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(png)?.into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use super::{Command, OnCommand, Status, decode};
    use anyhow::Context as _;
    use gpui_kit::{App, AsyncApp, Task};
    use std::cell::RefCell;
    use std::time::Duration;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    // macOS draws a template image in the menu bar's own color; Windows wants the colored icon.
    #[cfg(target_os = "macos")]
    const NORMAL: &[u8] = include_bytes!("../resources/tray/template.png");
    #[cfg(target_os = "macos")]
    const ATTENTION: &[u8] = include_bytes!("../resources/tray/template-attention.png");
    #[cfg(target_os = "windows")]
    const NORMAL: &[u8] = include_bytes!("../resources/tray/color.png");
    #[cfg(target_os = "windows")]
    const ATTENTION: &[u8] = include_bytes!("../resources/tray/color.png");

    fn icon(attention: bool) -> anyhow::Result<Icon> {
        let (rgba, width, height) = decode(if attention { ATTENTION } else { NORMAL })?;
        Icon::from_rgba(rgba, width, height).context("tray icon")
    }

    pub struct Tray {
        icon: TrayIcon,
        title: MenuItem,
        detail: MenuItem,
        sync: MenuItem,
        background: CheckMenuItem,
        open: MenuItem,
        settings: MenuItem,
        quit: MenuItem,
        last: RefCell<Status>,
        _events: Task<()>,
    }

    impl Tray {
        pub fn new(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
            let title = MenuItem::new(&status.title, false, None);
            let detail = MenuItem::new(&status.detail, false, None);
            let sync = MenuItem::new(&status.sync_now, status.can_sync, None);
            let background =
                CheckMenuItem::new(&status.background_label, status.background_enabled, status.background, None);
            let open = MenuItem::new(&status.open, true, None);
            let settings = MenuItem::new(&status.settings, true, None);
            let quit = MenuItem::new(&status.quit, true, None);
            let menu = Menu::new();
            menu.append_items(&[
                &title,
                &detail,
                &PredefinedMenuItem::separator(),
                &sync,
                &background,
                &PredefinedMenuItem::separator(),
                &open,
                &settings,
                &PredefinedMenuItem::separator(),
                &quit,
            ])?;
            let icon = TrayIconBuilder::new()
                .with_tooltip(status.tooltip())
                .with_icon(icon(status.attention)?)
                .with_icon_as_template(cfg!(target_os = "macos"))
                .with_menu(Box::new(menu))
                // A menu bar item opens its menu on click; a Windows tray icon opens the app.
                .with_menu_on_left_click(cfg!(target_os = "macos"))
                .build()
                .context("creating the tray icon")?;

            let commands: Vec<(MenuId, Command)> = vec![
                (sync.id().clone(), Command::SyncNow),
                (background.id().clone(), Command::ToggleBackground),
                (open.id().clone(), Command::Open),
                (settings.id().clone(), Command::Settings),
                (quit.id().clone(), Command::Quit),
            ];
            let tray_id = icon.id().clone();
            let events = cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    let mut asked = Vec::new();
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if let Some((_, command)) = commands.iter().find(|(id, _)| *id == event.id) {
                            asked.push(*command);
                        }
                    }
                    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                        if let TrayIconEvent::Click {
                            id,
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                            && id == tray_id
                            && cfg!(target_os = "windows")
                        {
                            asked.push(Command::Open);
                        }
                    }
                    for command in asked {
                        let on_command = on_command.clone();
                        cx.update(|cx| on_command(command, cx));
                    }
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                }
            });
            Ok(Tray {
                icon,
                title,
                detail,
                sync,
                background,
                open,
                settings,
                quit,
                last: RefCell::new(status.clone()),
                _events: events,
            })
        }

        pub fn update(&self, status: &Status) {
            let mut last = self.last.borrow_mut();
            if *last == *status {
                return;
            }
            self.title.set_text(&status.title);
            self.detail.set_text(&status.detail);
            self.sync.set_text(&status.sync_now);
            self.sync.set_enabled(status.can_sync);
            self.background.set_text(&status.background_label);
            self.background.set_enabled(status.background_enabled);
            self.background.set_checked(status.background);
            self.open.set_text(&status.open);
            self.settings.set_text(&status.settings);
            self.quit.set_text(&status.quit);
            let _ = self.icon.set_tooltip(Some(status.tooltip()));
            if last.attention != status.attention
                && let Ok(image) = icon(status.attention)
            {
                let _ = self.icon.set_icon_with_as_template(Some(image), cfg!(target_os = "macos"));
            }
            *last = status.clone();
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Command, OnCommand, Status, decode};
    use gpui_kit::{App, AsyncApp, Task};
    use ksni::blocking::{Handle, TrayMethods as _};
    use ksni::menu::{CheckmarkItem, StandardItem};
    use std::sync::mpsc::{self, Sender};
    use std::time::Duration;

    struct Item {
        status: Status,
        icon: Vec<ksni::Icon>,
        commands: Sender<Command>,
    }

    impl Item {
        fn send(&self, command: Command) {
            let _ = self.commands.send(command);
        }
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            crate::APP_ID.into()
        }

        fn title(&self) -> String {
            crate::APP_NAME.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: self.status.tooltip(),
                description: self.status.detail.clone(),
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(Command::Open);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let s = &self.status;
            vec![
                StandardItem { label: s.title.clone(), enabled: false, ..Default::default() }.into(),
                StandardItem { label: s.detail.clone(), enabled: false, ..Default::default() }.into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.sync_now.clone(),
                    enabled: s.can_sync,
                    activate: Box::new(|this: &mut Self| this.send(Command::SyncNow)),
                    ..Default::default()
                }
                .into(),
                CheckmarkItem {
                    label: s.background_label.clone(),
                    enabled: s.background_enabled,
                    checked: s.background,
                    activate: Box::new(|this: &mut Self| this.send(Command::ToggleBackground)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.open.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Open)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: s.settings.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Settings)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.quit.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub struct Tray {
        handle: Handle<Item>,
        _events: Task<()>,
    }

    impl Tray {
        pub fn new(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
            let (rgba, width, height) = decode(include_bytes!("../resources/tray/color.png"))?;
            // StatusNotifierItem wants ARGB32 in network byte order.
            let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            let icon = vec![ksni::Icon { width: width as i32, height: height as i32, data }];
            let (commands, received) = mpsc::channel();
            let handle = Item { status: status.clone(), icon, commands }.spawn()?;
            let events = cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    while let Ok(command) = received.try_recv() {
                        let on_command = on_command.clone();
                        cx.update(|cx| on_command(command, cx));
                    }
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                }
            });
            Ok(Tray { handle, _events: events })
        }

        pub fn update(&self, status: &Status) {
            let status = status.clone();
            self.handle.update(move |item| {
                if item.status != status {
                    item.status = status;
                }
            });
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let _ = self.handle.shutdown();
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::{OnCommand, Status};
    use gpui_kit::App;

    pub struct Tray;

    impl Tray {
        pub fn new(_: &Status, _: OnCommand, _: &mut App) -> anyhow::Result<Tray> {
            anyhow::bail!("no system tray on this platform")
        }

        pub fn update(&self, _: &Status) {}
    }
}
