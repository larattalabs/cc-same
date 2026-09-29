//! A read-only picture of everything, shared by the CLI's `doctor` and the app.

use crate::config::{LastSync, PendingRestart, State};
use crate::ctx::Ctx;
use crate::desktop;
use crate::fsx;
use crate::model::*;
use crate::paths::Paths;
use crate::plan::build_plan;
use crate::scan;
use crate::service::{self, Heartbeat, ServiceStatus};
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct PartitionView {
    pub part: Partition,
    pub email: Option<String>,
    pub sessions: usize,
    pub archived: usize,
    pub markers: usize,
    /// Live sessions (anywhere in the group) this index does not have yet.
    pub missing: usize,
    pub unreadable: usize,
    pub loaded: bool,
    pub excluded: bool,
    pub error: Option<String>,
    /// Collection file -> number of items (None: unreadable).
    pub collections: Vec<(String, Option<usize>)>,
    pub unmanaged: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SurfaceView {
    pub surface: Surface,
    pub enabled: bool,
    pub partitions: Vec<PartitionView>,
    /// Live sessions across all included partitions.
    pub union: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Warning {
    /// Index folders turned into symlinks by another tool: Claude silently stops saving there.
    SymlinkedFolders { surface: Surface, count: usize },
    /// `<org>.bak-*` style folders from other tools.
    LeftoverFolders { surface: Surface, dirs: Vec<String> },
    /// Linked partitions belong to more than one organization.
    SpansOrgs { surface: Surface, orgs: usize },
    /// Claude Code deletes old transcripts; an index entry without one cannot be reopened.
    ShortRetention { days: Option<f64>, path: PathBuf },
    /// Sessions whose transcript is no longer on disk.
    MissingTranscripts { missing: usize, total: usize },
    /// Claude Desktop's data folder was not found.
    DesktopDataMissing { path: PathBuf },
}

#[derive(Clone, Debug, Default)]
pub struct PlanView {
    /// Pending actions per partition, grouped by a readable kind.
    pub per_partition: Vec<(Partition, BTreeMap<&'static str, usize>)>,
    pub total: usize,
    /// Distinct sessions that would be copied or updated somewhere.
    pub sessions: usize,
    pub deferred: BTreeMap<String, usize>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Overview {
    pub desktop_version: Option<String>,
    pub app: AppState,
    pub labels: BTreeMap<String, String>,
    pub surfaces: Vec<SurfaceView>,
    pub plan: PlanView,
    pub warnings: Vec<Warning>,
    pub retention_days: Option<f64>,
    pub service: ServiceStatus,
    pub heartbeat: Option<Heartbeat>,
    pub last_sync: Option<LastSync>,
    pub baseline: Option<String>,
    pub pending_restart: Option<PendingRestart>,
    pub config: crate::config::Config,
}

impl Overview {
    pub fn surface(&self, s: Surface) -> Option<&SurfaceView> {
        self.surfaces.iter().find(|v| v.surface == s)
    }

    /// The restart hint, if it still applies (Desktop has not reloaded since).
    pub fn restart_hint(&self) -> Option<&PendingRestart> {
        let hint = self.pending_restart.as_ref()?;
        (self.app.running && self.app.init_marker == hint.init_marker).then_some(hint)
    }
}

pub fn kind_label(a: &Action) -> &'static str {
    match a.kind {
        ActionKind::WriteRecord if a.additive => "new sessions",
        ActionKind::WriteRecord => "updated sessions",
        ActionKind::TrashRecord => "deleted sessions",
        ActionKind::WriteTomb => "deletion markers",
        ActionKind::TrashTomb => "stale deletion markers",
        ActionKind::WriteCollection => "merged task lists",
        ActionKind::WriteArchiveIdx => "archive hint",
        ActionKind::SyncSidecar => "Cowork folders",
        ActionKind::TrashSidecar => "deleted Cowork folders",
    }
}

pub fn summarize(plan: &Plan) -> PlanView {
    let mut per: Vec<(Partition, BTreeMap<&'static str, usize>)> = Vec::new();
    for a in &plan.actions {
        let idx = match per.iter().position(|(p, _)| *p == a.part) {
            Some(i) => i,
            None => {
                per.push((a.part.clone(), BTreeMap::new()));
                per.len() - 1
            }
        };
        *per[idx].1.entry(kind_label(a)).or_default() += 1;
    }
    let sessions: std::collections::BTreeSet<&str> =
        plan.actions.iter().filter(|a| a.kind == ActionKind::WriteRecord).map(|a| a.name.as_str()).collect();
    PlanView {
        per_partition: per,
        total: plan.actions.len(),
        sessions: sessions.len(),
        deferred: plan.deferred.clone(),
        notes: plan.notes.clone(),
    }
}

pub fn overview(ctx: &Ctx) -> Overview {
    let cfg = ctx.config();
    let app = desktop::detect(ctx);
    let state = State::load(&ctx.paths);
    let labels = scan::account_labels(ctx);
    let mut warnings = Vec::new();
    if !ctx.paths.user_data.is_dir() {
        warnings.push(Warning::DesktopDataMissing { path: ctx.paths.user_data.clone() });
    }
    let mut surfaces = Vec::new();
    for surface in Surface::ALL {
        let enabled = cfg.syncs(surface);
        let parts = scan::discover(ctx, surface);
        if parts.is_empty() {
            continue;
        }
        let states: Vec<PartState> = parts.iter().map(|p| scan::scan_partition(ctx, p)).collect();
        let included: Vec<PartState> =
            states.iter().filter(|s| !cfg.is_excluded(&s.part.acct, &s.part.org)).cloned().collect();
        let union = scan::union_of(&included);
        let mut views = Vec::new();
        for s in &states {
            views.push(PartitionView {
                email: labels.get(&s.part.acct).cloned(),
                sessions: s.live_sessions(),
                archived: s.archived_sessions(),
                markers: s.tombs.len(),
                missing: union.iter().filter(|u| !s.records.contains_key(*u)).count(),
                unreadable: s.records.values().filter(|r| r.data.is_none()).count(),
                loaded: app.loaded(&s.part),
                excluded: cfg.is_excluded(&s.part.acct, &s.part.org),
                error: s.error.clone(),
                collections: s
                    .collections
                    .iter()
                    .map(|(rel, c)| {
                        (
                            rel.to_string(),
                            c.data.as_ref().map(|d| d.values().find_map(|v| v.as_array().map(Vec::len)).unwrap_or(0)),
                        )
                    })
                    .collect(),
                unmanaged: s.unmanaged.clone(),
                part: s.part.clone(),
            });
        }
        let links = states.iter().filter(|s| s.part.is_link).count();
        if links > 0 {
            warnings.push(Warning::SymlinkedFolders { surface, count: links });
        }
        let left = scan::leftover_dirs(ctx, surface);
        if !left.is_empty() {
            warnings.push(Warning::LeftoverFolders { surface, dirs: left });
        }
        if enabled {
            let orgs: std::collections::BTreeSet<&str> = included.iter().map(|s| s.part.org.as_str()).collect();
            if orgs.len() > 1 {
                warnings.push(Warning::SpansOrgs { surface, orgs: orgs.len() });
            }
        }
        if enabled && surface == Surface::Code {
            if let Some(tx) = scan::transcript_index(ctx) {
                let mut sessions: BTreeMap<&str, Option<&str>> = BTreeMap::new();
                for s in &included {
                    for (u, r) in &s.records {
                        if let Some(d) = &r.data {
                            sessions.insert(u, d.get("cliSessionId").and_then(Value::as_str));
                        }
                    }
                }
                let missing = sessions.values().filter(|c| !c.is_some_and(|c| tx.contains(c))).count();
                if missing > 0 {
                    warnings.push(Warning::MissingTranscripts { missing, total: sessions.len() });
                }
            }
        }
        surfaces.push(SurfaceView { surface, enabled, partitions: views, union: union.len() });
    }
    let retention_days = cleanup_period_days(&ctx.paths);
    if retention_days.is_none_or(|d| d < 365.0) {
        warnings.push(Warning::ShortRetention { days: retention_days, path: ctx.paths.claude_settings.clone() });
    }
    let (plan, _, _) = build_plan(ctx, Some(app.clone()), Some(&state));
    Overview {
        desktop_version: desktop::desktop_version(),
        labels,
        surfaces,
        plan: summarize(&plan),
        warnings,
        retention_days,
        service: service::status(ctx),
        heartbeat: Heartbeat::read(&ctx.paths),
        last_sync: state.last_sync.clone(),
        baseline: state.baseline.clone(),
        pending_restart: state.pending_restart.clone(),
        config: cfg,
        app,
    }
}

/// `cleanupPeriodDays` from Claude Code's settings (`None`: not set, the default is 30).
pub fn cleanup_period_days(paths: &Paths) -> Option<f64> {
    let Ok(Value::Object(s)) = fsx::read_json(&paths.claude_settings, 16 * 1024 * 1024) else { return None };
    s.get("cleanupPeriodDays").and_then(Value::as_f64)
}

/// Set `cleanupPeriodDays`, keeping every other setting. The previous file is kept next to it
/// as `settings.json.cc-same-backup-<time>`. Returns that backup's path (if there was a file).
pub fn set_cleanup_period_days(paths: &Paths, days: u32) -> Result<Option<PathBuf>> {
    anyhow::ensure!(days >= 1, "cleanupPeriodDays must be at least 1 (0 is rejected by Claude Code)");
    let path = &paths.claude_settings;
    let mut settings = Map::new();
    let mut backup = None;
    if path.exists() {
        match fsx::read_json(path, 16 * 1024 * 1024).with_context(|| format!("cannot read {}", path.display()))? {
            Value::Object(m) => settings = m,
            _ => anyhow::bail!("{} is not a JSON object; left untouched", path.display()),
        }
        let mut name = path.file_name().unwrap().to_os_string();
        name.push(format!(".cc-same-backup-{}", crate::snapshot::stamp()));
        let b = path.with_file_name(name);
        fs::copy(path, &b)?;
        backup = Some(b);
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    settings.insert("cleanupPeriodDays".into(), Value::from(days));
    let mut body = serde_json::to_vec_pretty(&Value::Object(settings))?;
    body.push(b'\n');
    fsx::atomic_write(path, &body, None, false)?;
    Ok(backup)
}

/// Days since the Unix epoch seconds `t`, for "2 min ago" style labels.
pub fn ago(t: f64) -> String {
    let secs = (fsx::now_secs() - t).max(0.0);
    match secs {
        s if s < 45.0 => "just now".into(),
        s if s < 90.0 => "a minute ago".into(),
        s if s < 3600.0 => format!("{} min ago", (s / 60.0).round() as u64),
        s if s < 5400.0 => "an hour ago".into(),
        s if s < 86400.0 => format!("{} hours ago", (s / 3600.0).round() as u64),
        s if s < 172800.0 => "yesterday".into(),
        s => format!("{} days ago", (s / 86400.0).round() as u64),
    }
}
