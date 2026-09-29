//! What the window and the tray show, and what they can do. There is one per app, alive whether
//! or not the window is open, so the tray stays current with the window closed.

use crate::i18n::{t, tf, tn};
use crate::model::{self, Account, Busy, Mood};
use crate::theme::ThemeChoice;
use crate::update::{self, Failure, Problem, Release};
use cc_same_core::config::Config;
use cc_same_core::report::{self, Overview};
use cc_same_core::retention::{self, Kept};
use cc_same_core::snapshot::{self, Manifest};
use cc_same_core::{ActionKind, Ctx, apply, desktop, fsx, service, watch};
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
const AGENT_NAME: &str = "cc-same-agent.exe";
#[cfg(not(windows))]
const AGENT_NAME: &str = "cc-same-agent";

/// How often the app looks for a new version, and how soon it looks again when it could not.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Something the user should hear about: the outcome of a task they started.
pub enum StoreEvent {
    Done(Result<String, String>),
}

/// Where an update stands.
#[derive(Clone, Debug, Default)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    /// The bytes downloaded so far.
    Downloading(Arc<AtomicU64>),
    /// Downloaded and checked: the new copy waits to be swapped in.
    Ready(PathBuf),
    Installing,
}

/// New versions: what GitHub said last, and what is under way.
#[derive(Clone, Debug, Default)]
pub struct Updates {
    /// A release newer than this copy.
    pub available: Option<Release>,
    pub phase: UpdatePhase,
    /// When GitHub last answered (Unix seconds).
    pub checked_at: Option<f64>,
    /// What went wrong last, and the error behind it.
    pub problem: Option<(Problem, String)>,
    /// The version this copy replaced, until the note about it is put away.
    pub updated_from: Option<String>,
    next_check: Option<Instant>,
}

impl Updates {
    /// Checking, downloading or installing.
    pub fn working(&self) -> bool {
        matches!(self.phase, UpdatePhase::Checking | UpdatePhase::Downloading(_) | UpdatePhase::Installing)
    }

    /// How much of the download is done, from 0 to 1.
    pub fn progress(&self) -> Option<f32> {
        let UpdatePhase::Downloading(bytes) = &self.phase else { return None };
        let total = self.available.as_ref()?.asset.as_ref()?.size.max(1);
        Some((bytes.load(Ordering::Relaxed) as f64 / total as f64).clamp(0., 1.) as f32)
    }
}

pub struct Store {
    pub ctx: Arc<Ctx>,
    pub overview: Option<Arc<Overview>>,
    pub snapshots: Vec<Manifest>,
    pub busy: Option<Busy>,
    /// The app opens (hidden) at login.
    pub open_at_login: bool,
    /// When the background switch was last turned on: the agent gets a moment to start.
    pub switched_on_at: Option<Instant>,
    pub updates: Updates,
    refreshing: bool,
    /// The background agent was compared with this version once.
    agent_checked: bool,
    fingerprint: u64,
    pending_since: Option<Instant>,
}

impl EventEmitter<StoreEvent> for Store {}

struct GlobalStore(Entity<Store>);

impl Global for GlobalStore {}

impl Store {
    fn blank(ctx: Arc<Ctx>) -> Store {
        Store {
            ctx,
            overview: None,
            snapshots: Vec::new(),
            busy: None,
            open_at_login: false,
            switched_on_at: None,
            updates: Updates::default(),
            refreshing: false,
            agent_checked: false,
            fingerprint: 0,
            pending_since: None,
        }
    }

    /// The live store: reads now, then again whenever anything it depends on changes.
    pub fn init(ctx: Arc<Ctx>, cx: &mut App) -> Entity<Store> {
        let store = cx.new(|cx| {
            let mut store = Store::blank(ctx);
            store.open_at_login = crate::login::enabled();
            store.updates.updated_from = update::updated_from(&store.ctx.paths).map(|v| v.to_string());
            store.refresh(cx);
            store.watch_for_changes(cx);
            store.watch_for_updates(cx);
            store
        });
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    /// A still picture of a given state (screenshots): no polling and no disk access.
    pub fn preview(
        ctx: Arc<Ctx>,
        overview: Option<Overview>,
        snapshots: Vec<Manifest>,
        busy: Option<Busy>,
        cx: &mut App,
    ) -> Entity<Store> {
        let store = cx.new(|_| Store { overview: overview.map(Arc::new), snapshots, busy, ..Store::blank(ctx) });
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    pub fn global(cx: &App) -> Entity<Store> {
        cx.global::<GlobalStore>().0.clone()
    }

    pub fn config(&self) -> Config {
        self.ctx.config()
    }

    pub fn theme(&self) -> ThemeChoice {
        ThemeChoice::from_config(&self.ctx.config().appearance)
    }

    pub fn mood(&self) -> Mood {
        model::mood(self.overview.as_deref(), self.busy == Some(Busy::Syncing), self.pending_since.map(|t| t.elapsed()))
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.overview.as_deref().map(model::accounts).unwrap_or_default()
    }

    /// The background agent is registered.
    pub fn background_on(&self) -> bool {
        self.overview.as_ref().is_some_and(|o| o.service.installed)
    }

    // ------------------------------------------------------------------ reading

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let (overview, snapshots) =
                cx.background_executor().spawn(async move { (report::overview(&ctx), snapshot::list(&ctx)) }).await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing = false;
                this.pending_since = match (overview.plan.total > 0, this.pending_since) {
                    (true, None) => Some(Instant::now()),
                    (true, since) => since,
                    (false, _) => None,
                };
                this.overview = Some(Arc::new(overview));
                this.snapshots = snapshots;
                this.renew_agent(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read when Claude's session folders, Claude itself, our state or the agent change,
    /// and at least every 12 seconds.
    fn watch_for_changes(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                ticks += 1;
                let Ok((ctx, last)) = this.update(cx, |this, _| (this.ctx.clone(), this.fingerprint)) else { break };
                let fp = cx.background_executor().spawn(async move { fingerprint(&ctx) }).await;
                if fp != last || ticks.is_multiple_of(8) {
                    let alive = this.update(cx, |this, cx| {
                        this.fingerprint = fp;
                        this.refresh(cx);
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------ doing

    /// Run blocking work off the UI thread, then report and re-read.
    fn run_task(
        &mut self,
        busy: Busy,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Ctx) -> anyhow::Result<String> + Send + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy);
        cx.notify();
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work(&ctx) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                cx.emit(StoreEvent::Done(result.map_err(|e| format!("{e:#}"))));
                this.refresh(cx);
            });
        })
        .detach();
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Syncing, cx, |ctx| {
            let (_, out) = apply::run_sync(ctx, "app")?;
            if !out.errors.is_empty() {
                anyhow::bail!("{}", tn("toast.write_errors", out.errors.len(), &[]));
            }
            let copied = out.applied.get(&ActionKind::WriteRecord).copied().unwrap_or(0);
            Ok(match (copied, out.applied_total()) {
                (0, 0) => t("toast.in_sync"),
                (0, n) => tn("toast.applied", n, &[]),
                (n, _) => tn("toast.copied", n, &[]),
            })
        });
    }

    pub fn set_background(&mut self, on: bool, cx: &mut Context<Self>) {
        self.switched_on_at = on.then(Instant::now);
        self.run_task(Busy::Service, cx, move |ctx| {
            if on {
                if !ctx.paths.config_file().exists() {
                    ctx.config().save(&ctx.paths)?;
                }
                let exe = std::env::current_exe()?;
                service::install(ctx, &exe, AGENT_NAME)?;
            } else {
                service::uninstall(ctx)?;
            }
            // The switch shows the result.
            Ok(String::new())
        });
    }

    pub fn set_open_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        self.open_at_login = on;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { crate::login::set(on) }).await;
            let _ = this.update(cx, |this, cx| {
                this.open_at_login = crate::login::enabled();
                if let Err(e) = result {
                    cx.emit(StoreEvent::Done(Err(format!("{e:#}"))));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn restore(&mut self, id: String, cx: &mut Context<Self>) {
        self.run_task(Busy::Restoring, cx, move |ctx| {
            snapshot::restore(ctx, &id)?;
            Ok(t("toast.restored"))
        });
    }

    pub fn fix_links(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Fixing, cx, |ctx| {
            let fixed = snapshot::fix_symlinks(ctx)?;
            apply::run_sync(ctx, "app")?;
            Ok(tn("toast.fixed", fixed.len(), &[]))
        });
    }

    /// Keep the transcripts behind Desktop sessions, whichever setting it takes.
    pub fn keep_transcripts(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, |ctx| {
            Ok(match retention::keep(&ctx.paths)? {
                Kept::TenYears => t("toast.kept"),
                Kept::AnyAge | Kept::Already => t("toast.kept_unlimited"),
            })
        });
    }

    /// Put back what CC Same changed in Claude Code's settings.
    pub fn undo_retention(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, |ctx| {
            retention::undo(&ctx.paths)?;
            Ok(t("toast.undone"))
        });
    }

    /// The background agent is a copy of the app from when it was switched on. After an update it
    /// still runs the old version, so it is replaced with this one (once, at the first reading).
    fn renew_agent(&mut self, cx: &mut Context<Self>) {
        let Some(overview) = self.overview.clone() else { return };
        if std::mem::replace(&mut self.agent_checked, true) {
            return;
        }
        let behind = overview
            .heartbeat
            .as_ref()
            .and_then(|h| semver::Version::parse(&h.version).ok())
            .is_some_and(|v| v < update::current());
        if overview.service.installed && behind {
            self.set_background(true, cx);
        }
    }

    // ------------------------------------------------------------------ updates

    /// Look for a new version a few seconds after starting and then once a day, and install a
    /// downloaded one while nobody is looking.
    fn watch_for_updates(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(8)).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    let due = this.updates.next_check.is_none_or(|at| Instant::now() >= at);
                    if due && this.config().check_updates && update::checks_automatically() {
                        this.check_for_updates(false, cx);
                    }
                    this.install_when_idle(cx);
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(10 * 60)).await;
            }
        })
        .detach();
    }

    /// Ask GitHub for the latest version. `manual` reports the answer either way.
    pub fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if !matches!(self.updates.phase, UpdatePhase::Idle) {
            return;
        }
        self.updates.phase = UpdatePhase::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { update::check() }).await;
            let _ = this.update(cx, |this, cx| this.checked(result, manual, cx));
        })
        .detach();
    }

    fn checked(&mut self, result: update::Outcome<Option<Release>>, manual: bool, cx: &mut Context<Self>) {
        self.updates.phase = UpdatePhase::Idle;
        match result {
            Ok(release) => {
                self.updates.checked_at = Some(fsx::now_secs());
                self.updates.next_check = Some(Instant::now() + CHECK_EVERY);
                self.updates.problem = None;
                self.updates.available = release;
                match &self.updates.available {
                    Some(release) => {
                        if self.config().auto_update && release.asset.is_some() && update::target().is_ok() {
                            self.download_update(false, cx);
                        }
                    }
                    None if manual => {
                        cx.emit(StoreEvent::Done(Ok(tf("update.up_to_date", &[("version", &update::VERSION)]))));
                    }
                    None => {}
                }
            }
            Err(failure) => {
                self.updates.next_check = Some(Instant::now() + RETRY_AFTER);
                eprintln!("checking for updates: {:#}", failure.error);
                if manual {
                    cx.emit(StoreEvent::Done(Err(t(failure.problem.message_key()))));
                }
                // With a release already found, its note stays as it is.
                if self.updates.available.is_none() {
                    self.updates.problem = Some((failure.problem, failure.error.root_cause().to_string()));
                }
            }
        }
        cx.notify();
    }

    /// Update now: install what is downloaded, download what is available (then install it), or
    /// look for something new.
    pub fn update_or_check(&mut self, cx: &mut Context<Self>) {
        match (&self.updates.phase, &self.updates.available) {
            (UpdatePhase::Ready(staged), _) => {
                let staged = staged.clone();
                self.install_update(staged, false, cx);
            }
            (UpdatePhase::Idle, Some(_)) => self.download_update(true, cx),
            (UpdatePhase::Idle, None) => self.check_for_updates(true, cx),
            _ => {}
        }
    }

    /// Download the available release and check it; `then_install` swaps it in right after.
    fn download_update(&mut self, then_install: bool, cx: &mut Context<Self>) {
        let Some(release) = self.updates.available.clone() else { return };
        if !matches!(self.updates.phase, UpdatePhase::Idle) {
            return;
        }
        let Some(asset) = release.asset.clone() else {
            let error = anyhow::anyhow!("release {} has no archive for this system", release.version);
            return self.update_failed(Failure { problem: Problem::NoBuild, error }, cx);
        };
        let target = match update::target() {
            Ok(target) => target,
            Err(failure) => return self.update_failed(failure, cx),
        };
        let bytes = Arc::new(AtomicU64::new(0));
        self.updates.phase = UpdatePhase::Downloading(bytes.clone());
        self.updates.problem = None;
        cx.notify();
        let dir = update::updates_dir(&self.ctx.paths);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let archive = update::download(&asset, &dir, &bytes)?;
                    update::unpack(&archive, &release.version, &target)
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(staged) => {
                    this.updates.phase = UpdatePhase::Ready(staged.clone());
                    if then_install {
                        this.install_update(staged, false, cx);
                    } else {
                        this.install_when_idle(cx);
                    }
                    cx.notify();
                }
                Err(failure) => this.update_failed(failure, cx),
            });
        })
        .detach();
        // Redraw the progress while it downloads.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let downloading = this.update(cx, |this, cx| {
                    let downloading = matches!(this.updates.phase, UpdatePhase::Downloading(_));
                    if downloading {
                        cx.notify();
                    }
                    downloading
                });
                if !matches!(downloading, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Swap the downloaded copy in and restart into it; `hidden` restarts without the window.
    fn install_update(&mut self, staged: PathBuf, hidden: bool, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let target = match update::target() {
            Ok(target) => target,
            Err(failure) => return self.update_failed(failure, cx),
        };
        self.updates.phase = UpdatePhase::Installing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { update::install(&staged, &target, hidden) }).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(failure) => this.update_failed(failure, cx),
            });
        })
        .detach();
    }

    /// With automatic updates on, install a downloaded one once the window is closed and nothing
    /// is running.
    pub fn install_when_idle(&mut self, cx: &mut Context<Self>) {
        if let UpdatePhase::Ready(staged) = &self.updates.phase
            && self.config().auto_update
            && self.busy.is_none()
            && cx.windows().is_empty()
        {
            let staged = staged.clone();
            self.install_update(staged, true, cx);
        }
    }

    /// With automatic updates on, put a downloaded update in place on the way out; the next start
    /// is the new version.
    pub fn install_on_quit(&mut self) {
        if let UpdatePhase::Ready(staged) = &self.updates.phase
            && self.config().auto_update
            && self.busy.is_none()
        {
            let installed = update::target().and_then(|target| update::install_on_quit(staged, &target));
            if let Err(failure) = installed {
                eprintln!("updating on quit: {:#}", failure.error);
            }
        }
    }

    fn update_failed(&mut self, failure: Failure, cx: &mut Context<Self>) {
        eprintln!("updating: {:#}", failure.error);
        self.updates.phase = UpdatePhase::Idle;
        self.updates.problem = Some((failure.problem, failure.error.root_cause().to_string()));
        cx.notify();
    }

    /// Put away the note that this copy was just updated.
    pub fn dismiss_updated(&mut self, cx: &mut Context<Self>) {
        if self.updates.updated_from.take().is_some() {
            cx.notify();
        }
    }

    /// Change a setting now and save it in the background.
    pub fn update_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Config)) {
        let mut cfg = self.ctx.config();
        change(&mut cfg);
        self.ctx.set_config(cfg.clone());
        cx.notify();
        let ctx = self.ctx.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = cfg.save(&ctx.paths) {
                    ctx.log(format!("saving settings: {e:#}"));
                }
            })
            .detach();
    }
}

/// Changes worth re-reading for: Claude's session folders and state, and ours.
fn fingerprint(ctx: &Ctx) -> u64 {
    let mut h = DefaultHasher::new();
    watch::fingerprint(ctx, desktop::is_running(ctx)).hash(&mut h);
    let paths = &ctx.paths;
    for p in [paths.state_file(), paths.heartbeat_file(), paths.config_file(), paths.claude_settings.clone()] {
        if let Ok(md) = std::fs::metadata(&p) {
            fsx::mtime_ns(&md).hash(&mut h);
        }
    }
    h.finish()
}
