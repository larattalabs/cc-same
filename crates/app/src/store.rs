//! What the window and the tray show, and what they can do. There is one per app, alive whether
//! or not the window is open, so the tray stays current with the window closed.

use crate::i18n::{t, tn};
use crate::model::{self, Account, Busy, Mood};
use crate::theme::ThemeChoice;
use cc_same_core::config::Config;
use cc_same_core::report::{self, Overview};
use cc_same_core::snapshot::{self, Manifest};
use cc_same_core::{ActionKind, Ctx, apply, desktop, fsx, service, watch};
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
const AGENT_NAME: &str = "cc-same-agent.exe";
#[cfg(not(windows))]
const AGENT_NAME: &str = "cc-same-agent";

/// Something the user should hear about: the outcome of a task they started.
pub enum StoreEvent {
    Done(Result<String, String>),
}

pub struct Store {
    pub ctx: Arc<Ctx>,
    pub overview: Option<Arc<Overview>>,
    pub snapshots: Vec<Manifest>,
    pub busy: Option<Busy>,
    /// The app opens (hidden) at login.
    pub open_at_login: bool,
    refreshing: bool,
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
            refreshing: false,
            fingerprint: 0,
            pending_since: None,
        }
    }

    /// The live store: reads now, then again whenever anything it depends on changes.
    pub fn init(ctx: Arc<Ctx>, cx: &mut App) -> Entity<Store> {
        let store = cx.new(|cx| {
            let mut store = Store::blank(ctx);
            store.open_at_login = crate::login::enabled();
            store.refresh(cx);
            store.watch_for_changes(cx);
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

    pub fn keep_transcripts(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, |ctx| {
            report::set_cleanup_period_days(&ctx.paths, 3650)?;
            Ok(t("toast.kept"))
        });
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
