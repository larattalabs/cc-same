//! The menu bar (macOS) and notification area (Windows, Linux) icon: the headline at a glance,
//! Sync now, the background switch, and the way back to the window.

use crate::i18n::{t, tf};
use crate::model::Busy;
use crate::store::{Store, UpdatePhase};
use gpui_kit::App;
use std::rc::Rc;

/// What a click in the tray asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    SyncNow,
    ToggleBackground,
    Settings,
    /// Install the new version, or look for one.
    Update,
    /// Switch Claude to this account.
    SwitchTo(String),
    /// Restart Claude on its sign-in page, to sign in to another account.
    SignInAnother,
    Quit,
}

/// An account in the tray's "Switch Account" menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayAccount {
    pub id: String,
    pub name: String,
    /// Claude is signed in to it.
    pub current: bool,
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
    /// "Check for Updates…", or "Update to 0.2.0…" once there is one.
    pub update: String,
    pub update_enabled: bool,
    /// Switching accounts is available on this system.
    pub can_switch: bool,
    /// The account Claude is signed in to, then the ones it can switch to.
    pub accounts: Vec<TrayAccount>,
    pub switch_label: String,
    pub sign_in_label: String,
    pub quit: String,
}

impl Status {
    pub fn of(store: &Store) -> Status {
        let mood = store.mood();
        let idle = store.busy.is_none();
        let (update, update_enabled) = match (&store.updates.phase, &store.updates.available) {
            (UpdatePhase::Checking, _) => (t("app.checking_updates"), false),
            (UpdatePhase::Downloading(_) | UpdatePhase::Installing, _) => (t("app.updating"), false),
            (_, Some(release)) => (tf("app.update_to", &[("version", &release.version)]), true),
            (_, None) => (t("app.check_updates"), true),
        };
        let logins = store.overview.as_ref().map(|o| o.logins.clone()).unwrap_or_default();
        let accounts = logins
            .signed_in
            .iter()
            .map(|id| TrayAccount { id: id.clone(), name: store.account_name(id), current: true })
            .chain(logins.saved.iter().map(|s| TrayAccount {
                id: s.account.clone(),
                name: store.account_name(&s.account),
                current: false,
            }))
            .collect();
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
            update,
            update_enabled,
            can_switch: logins.supported,
            accounts,
            switch_label: t("tray.switch_account"),
            sign_in_label: t("tray.sign_in_another"),
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
    use std::rc::Rc;
    use std::time::Duration;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
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
        update: MenuItem,
        quit: MenuItem,
        /// "Switch Account", when switching is available.
        accounts: Option<Submenu>,
        /// What each item of the accounts menu does; they change with the accounts.
        account_commands: Rc<RefCell<Vec<(MenuId, Command)>>>,
        last: RefCell<Status>,
        _events: Task<()>,
    }

    /// Fill the accounts menu: the account in use (checked), the ones to switch to, then signing
    /// in to another.
    fn fill_accounts(menu: &Submenu, status: &Status, commands: &RefCell<Vec<(MenuId, Command)>>) {
        while menu.remove_at(0).is_some() {}
        let mut found = Vec::new();
        for account in &status.accounts {
            let item = CheckMenuItem::new(&account.name, !account.current && status.can_sync, account.current, None);
            let _ = menu.append(&item);
            if !account.current {
                found.push((item.id().clone(), Command::SwitchTo(account.id.clone())));
            }
        }
        if !status.accounts.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let sign_in = MenuItem::new(&status.sign_in_label, status.can_sync, None);
        let _ = menu.append(&sign_in);
        found.push((sign_in.id().clone(), Command::SignInAnother));
        *commands.borrow_mut() = found;
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
            let update = MenuItem::new(&status.update, status.update_enabled, None);
            let quit = MenuItem::new(&status.quit, true, None);
            let account_commands = Rc::new(RefCell::new(Vec::new()));
            let accounts = status.can_switch.then(|| {
                let menu = Submenu::new(&status.switch_label, true);
                fill_accounts(&menu, status, &account_commands);
                menu
            });
            let menu = Menu::new();
            menu.append_items(&[&title, &detail, &PredefinedMenuItem::separator(), &sync, &background])?;
            if let Some(accounts) = &accounts {
                menu.append(accounts)?;
            }
            menu.append_items(&[
                &PredefinedMenuItem::separator(),
                &open,
                &settings,
                &update,
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
                (update.id().clone(), Command::Update),
                (quit.id().clone(), Command::Quit),
            ];
            let tray_id = icon.id().clone();
            let dynamic = account_commands.clone();
            let events = cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    let mut asked = Vec::new();
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        let known = commands.iter().find(|(id, _)| *id == event.id).map(|(_, c)| c.clone());
                        let known = known.or_else(|| {
                            dynamic.borrow().iter().find(|(id, _)| *id == event.id).map(|(_, c)| c.clone())
                        });
                        asked.extend(known);
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
                update,
                quit,
                accounts,
                account_commands,
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
            self.update.set_text(&status.update);
            self.update.set_enabled(status.update_enabled);
            self.quit.set_text(&status.quit);
            if let Some(accounts) = &self.accounts {
                accounts.set_text(&status.switch_label);
                let changed = (&last.accounts, &last.sign_in_label, last.can_sync)
                    != (&status.accounts, &status.sign_in_label, status.can_sync);
                if changed {
                    fill_accounts(accounts, status, &self.account_commands);
                }
            }
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
    use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
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

        /// "Switch Account": the account in use (checked), the ones to switch to, then signing in
        /// to another.
        fn accounts_menu(&self) -> SubMenu<Self> {
            let s = &self.status;
            let mut submenu: Vec<ksni::MenuItem<Self>> = s
                .accounts
                .iter()
                .map(|account| {
                    let id = account.id.clone();
                    CheckmarkItem {
                        label: account.name.clone(),
                        enabled: !account.current && s.can_sync,
                        checked: account.current,
                        activate: Box::new(move |this: &mut Self| this.send(Command::SwitchTo(id.clone()))),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            if !s.accounts.is_empty() {
                submenu.push(ksni::MenuItem::Separator);
            }
            submenu.push(
                StandardItem {
                    label: s.sign_in_label.clone(),
                    enabled: s.can_sync,
                    activate: Box::new(|this: &mut Self| this.send(Command::SignInAnother)),
                    ..Default::default()
                }
                .into(),
            );
            SubMenu { label: s.switch_label.clone(), submenu, ..Default::default() }
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
            let accounts: Option<ksni::MenuItem<Self>> = s.can_switch.then(|| self.accounts_menu().into());
            let mut items: Vec<ksni::MenuItem<Self>> = vec![
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
                StandardItem {
                    label: s.update.clone(),
                    enabled: s.update_enabled,
                    activate: Box::new(|this: &mut Self| this.send(Command::Update)),
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
            ];
            // After the background switch: title, detail, separator, Sync now, background.
            if let Some(accounts) = accounts {
                items.insert(5, accounts);
            }
            items
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
