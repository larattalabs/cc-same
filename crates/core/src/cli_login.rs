//! Taking Claude Code's command line along when Claude Desktop switches accounts (`switchCli`,
//! off unless turned on; macOS).
//!
//! The command line keeps a sign-in of its own, apart from Desktop's: a JSON object in the
//! keychain (`Claude Code-credentials`, under the user's name) and who it belongs to in
//! `oauthAccount` of `~/.claude.json`. Most of that object is the account's (`claudeAiOauth`, the
//! login itself, and anything not known to be shared); a few fields are the machine's, shared by
//! every account (the sign-ins of MCP servers and plugins, [`SHARED_KEYS`]).
//!
//! A switch sets the account's part aside in the login keychain, under `cc-same CLI sign-in` and
//! the account's ID, and puts back the part kept for the account Claude switched to, next to the
//! machine's current shared fields. Nothing ever leaves the keychain for a file, and nothing is
//! sent anywhere.
//!
//! * The sign-in set aside is the one in use, whoever it belongs to.
//! * When nothing is kept for the account switched to yet, the command line is signed out (the
//!   shared fields stay), its sign-in kept like any other, and `claude` asks to sign in: `/login`
//!   there, once, to that account. Signing in over the old sign-in instead would end it with
//!   nothing kept. A command line signed out this way is signed back in by switching back.
//! * The account signed in never has a copy kept as well: tokens rotate as they are used, so a
//!   second copy would go stale, and putting a stale one back signs the account out. A copy put
//!   back is forgotten before the switch counts as done, and set aside afresh when it is left.
//! * A journal makes a switch all or nothing. It names who from and who to, how far the switch
//!   got, and a fingerprint (SHA-256) of the login each side had, never a secret. One that was
//!   interrupted is settled before anything else is done, and only ever by what the fingerprints
//!   prove: when the login in use is exactly the one before, or exactly the one put in place, the
//!   switch is undone or finished. When it is neither (a `/login` since, or tokens refreshed),
//!   the login in use is left as it is and every copy that might be stale is forgotten. At worst,
//!   an account then needs `/login` once more; a stale login is never put back.
//! * Claude Code refreshes its tokens under two lock folders, and writes `~/.claude.json` under a
//!   third. A switch holds all three, keeping them fresh while it does, so a `claude` running
//!   meanwhile finishes a refresh first, or finds the new account's tokens after. Before every
//!   change it checks the locks are still its own; if one was taken over, it stops, and the
//!   journal settles the switch the next time. Only `oauthAccount` in `~/.claude.json` changes;
//!   the rest keeps its content (not its spacing).
//! * Only the default configuration folder: with `CLAUDE_CONFIG_DIR` set, Claude Code names its
//!   keychain item after that folder, and the command line is left alone.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::is_uuid;
use crate::secrets::{self, Stores};
use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Claude Code's own keychain item.
const LIVE_SERVICE: &str = "Claude Code-credentials";
/// Ours, one per account.
const KEPT_SERVICE: &str = "cc-same CLI sign-in";
/// Fields of Claude Code's item that belong to the machine, not to an account: whatever is in use
/// stays in use across a switch. (As claude-swap has them.)
const SHARED_KEYS: &[&str] = &["mcpOAuth", "mcpOAuthClientConfig", "mcpXaaIdp", "mcpXaaIdpConfig", "pluginSecrets"];
const JOURNAL: &str = "cli-switch.json";

/// What became of the command line's sign-in in a switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum Followed {
    /// Signed in to the account Claude switched to now (from `from`, or from signed out).
    Switched { from: Option<String> },
    /// Already signed in to it.
    AlreadyThere,
    /// Nothing kept for that account yet: signed out, the sign-in of `from` kept.
    SignedOut { from: String },
    /// Not signed in with a claude.ai login, and nothing kept to sign in with.
    NotSignedIn,
    /// Claude signed out to add an account: the command line stays signed in to `on`.
    Stayed { on: String },
}

/// A sign-in set aside: the account's part of Claude Code's item, and who it belongs to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Kept {
    credentials: Map<String, Value>,
    oauth_account: Map<String, Value>,
    set_aside_at: f64,
}

/// How far a switch got. Written before each step that changes something.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    /// Nothing changed yet, though the login in use may be being set aside.
    #[default]
    Started,
    /// The login in use is set aside; the one in place may have changed since.
    SetAside,
    /// Everything is in place; only forgetting the copy put back is left.
    Committed,
    /// The switch failed and is being undone.
    Undoing,
    /// The login from before is back in place; only forgetting its copy is left.
    Undone,
}

/// A switch in progress. Fingerprints, never secrets.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Journal {
    from: Option<String>,
    to: Option<String>,
    phase: Phase,
    /// The login in use before the switch (none: signed out).
    original: Option<String>,
    /// The login the switch puts in place (none: signing out).
    target: Option<String>,
}

/// Bring the command line to `to` (or leave it be, for `None`).
pub fn follow(ctx: &Ctx, to: Option<&str>) -> Result<Followed> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        bail!("CLAUDE_CONFIG_DIR is set, so Claude Code keeps its sign-in elsewhere; the command line was left alone");
    }
    let stores = secrets::open(ctx)?;
    let locks = Locks::take(ctx)?;
    follow_with(&Live { ctx, stores: &stores, user: user_name(), locks: &locks }, to)
}

/// Forget the command-line sign-in kept for `account`, wherever switching is possible at all.
pub fn forget(ctx: &Ctx, account: &str) -> Result<()> {
    match secrets::open(ctx) {
        Ok(stores) => stores.kept.delete(KEPT_SERVICE, account),
        // No keychain here: nothing can have been kept.
        Err(_) => Ok(()),
    }
}

/// Everything a switch reads and changes, with the locks it holds.
struct Live<'a> {
    ctx: &'a Ctx,
    stores: &'a Stores,
    user: String,
    locks: &'a Locks,
}

impl Live<'_> {
    fn config(&self) -> Result<Map<String, Value>> {
        read_config(&self.ctx.paths.claude_json)
    }

    /// Claude Code's item as an object, or `None`; an item that is not one is left alone.
    fn credentials(&self) -> Result<Option<Map<String, Value>>> {
        match self.stores.live.get(LIVE_SERVICE, &self.user)? {
            None => Ok(None),
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(o)) => Ok(Some(o)),
                _ => bail!("Claude Code's sign-in is not one CC Same knows how to switch (an API key?); the command line was left alone"),
            },
        }
    }

    /// The fingerprint of the login in place now, or `None` when there is none.
    fn current(&self) -> Result<Option<String>> {
        Ok(self.credentials()?.filter(|c| c.contains_key("claudeAiOauth")).map(|c| fingerprint(&c)))
    }

    fn kept(&self, account: &str) -> Result<Option<Kept>> {
        let Some(text) = self.stores.kept.get(KEPT_SERVICE, account)? else { return Ok(None) };
        let damaged = || {
            anyhow!("the command-line sign-in kept for {account} is damaged; sign in to it again with /login in claude")
        };
        // Never let a parse error carry the secret into a message or a log.
        let kept: Kept = serde_json::from_str(&text).map_err(|_| damaged())?;
        if !kept.credentials.contains_key("claudeAiOauth")
            || kept.oauth_account.get("accountUuid").and_then(Value::as_str) != Some(account)
        {
            return Err(damaged());
        }
        Ok(Some(kept))
    }

    fn keep(&self, account: &str, credentials: &Map<String, Value>, oauth_account: Map<String, Value>) -> Result<()> {
        self.locks.check()?;
        let kept = Kept { credentials: own_part(credentials), oauth_account, set_aside_at: fsx::now_secs() };
        self.stores.kept.set(KEPT_SERVICE, account, &ascii_json(&serde_json::to_value(kept)?))
    }

    fn forget(&self, account: &str) -> Result<()> {
        self.locks.check()?;
        self.stores.kept.delete(KEPT_SERVICE, account)
    }

    /// Put `kept`'s login in place (nobody's, for `None`), next to the machine's shared fields as
    /// they are now, and its `oauthAccount` in the config.
    fn put(&self, kept: Option<&Kept>) -> Result<()> {
        let shared = self.credentials()?.unwrap_or_default();
        let mut credentials: Map<String, Value> = kept.map(|k| own_part(&k.credentials)).unwrap_or_default();
        credentials.extend(shared_part(&shared));
        self.locks.check()?;
        if credentials.is_empty() {
            self.stores.live.delete(LIVE_SERVICE, &self.user)?;
        } else {
            self.stores.live.set(LIVE_SERVICE, &self.user, &ascii_json(&Value::Object(credentials)))?;
        }
        self.name(kept.map(|k| &k.oauth_account))
    }

    /// Say in the config who the command line is signed in to (nobody, for `None`).
    fn name(&self, oauth_account: Option<&Map<String, Value>>) -> Result<()> {
        let mut config = self.config()?;
        let wanted = oauth_account.map(|o| Value::Object(o.clone()));
        if config.get("oauthAccount") == wanted.as_ref() {
            return Ok(());
        }
        match wanted {
            Some(o) => config.insert("oauthAccount".into(), o),
            None => config.remove("oauthAccount"),
        };
        self.locks.check()?;
        write_config(&self.ctx.paths.claude_json, &config)
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        fsx::create_private_dir_all(&self.ctx.paths.state_dir)?;
        fsx::atomic_write(&journal_path(self.ctx), &serde_json::to_vec_pretty(journal)?, None, false)?;
        Ok(())
    }
}

fn follow_with(live: &Live, to: Option<&str>) -> Result<Followed> {
    settle(live)?;
    let config = live.config()?;
    let credentials = live.credentials()?;
    // Signed in: a claude.ai login, and who it belongs to.
    let on = credentials
        .as_ref()
        .filter(|c| c.contains_key("claudeAiOauth"))
        .and_then(|_| config.get("oauthAccount")?.get("accountUuid")?.as_str())
        .filter(|a| is_uuid(a))
        .map(str::to_string);
    let Some(to) = to else {
        return Ok(on.map(|on| Followed::Stayed { on }).unwrap_or(Followed::NotSignedIn));
    };
    if on.as_deref() == Some(to) {
        // The account signed in never has a copy kept as well.
        live.forget(to)?;
        return Ok(Followed::AlreadyThere);
    }
    let target = live.kept(to)?;
    if on.is_none() && target.is_none() {
        return Ok(Followed::NotSignedIn);
    }

    let mut journal = Journal {
        from: on.clone(),
        to: Some(to.to_string()),
        phase: Phase::Started,
        original: live.current()?,
        target: target.as_ref().map(|k| fingerprint(&k.credentials)),
    };
    live.save(&journal)?;
    if let (Some(from), Some(credentials)) = (&on, &credentials) {
        let oauth_account = config.get("oauthAccount").and_then(Value::as_object).cloned().unwrap_or_default();
        live.keep(from, credentials, oauth_account)?;
    }
    journal.phase = Phase::SetAside;
    live.save(&journal)?;
    if let Err(e) = live.put(target.as_ref()) {
        journal.phase = Phase::Undoing;
        // If even that cannot be written, the journal still says SetAside, which settles the same.
        if live.save(&journal).is_ok() {
            settle_with(live, journal).context("putting the command line's sign-in back")?;
        }
        return Err(e);
    }
    journal.phase = Phase::Committed;
    live.save(&journal)?;
    settle_with(live, journal)?;
    Ok(match (target, on) {
        (Some(_), from) => Followed::Switched { from },
        (None, Some(from)) => Followed::SignedOut { from },
        (None, None) => unreachable!("returned above"),
    })
}

/// Settle a switch that was interrupted, if there is one.
fn settle(live: &Live) -> Result<()> {
    match read_journal(live.ctx)? {
        Some(journal) => settle_with(live, journal),
        None => Ok(()),
    }
}

/// Take a switch from where its journal says it got to, to done. Each step can be taken again.
fn settle_with(live: &Live, mut journal: Journal) -> Result<()> {
    loop {
        let current = live.current()?;
        match journal.phase {
            Phase::Started => {
                // Nothing was put in place; a copy of the login in use may have been made, and the
                // login in use never has a copy kept as well.
                if let Some(from) = &journal.from {
                    live.forget(from)?;
                }
                return remove_journal(live.ctx);
            }
            Phase::SetAside if current == journal.original => {
                // The switch never got to put anything in place.
                journal.phase = Phase::Undone;
            }
            Phase::SetAside if current == journal.target => {
                // In place and not used since: finish it, the config too.
                let kept = match &journal.to {
                    Some(to) if journal.target.is_some() => {
                        Some(live.kept(to)?.ok_or_else(|| anyhow!("the sign-in put back is gone"))?)
                    }
                    _ => None,
                };
                live.name(kept.as_ref().map(|k| &k.oauth_account))?;
                journal.phase = Phase::Committed;
            }
            Phase::Undoing if current == journal.target && current != journal.original => {
                // Put the login from before back from its copy.
                let kept = match &journal.from {
                    Some(from) => Some(live.kept(from)?.ok_or_else(|| anyhow!("the sign-in set aside is gone"))?),
                    None => None,
                };
                if kept.as_ref().map(|k| fingerprint(&k.credentials)) != journal.original {
                    stale(live, &journal)?;
                    return remove_journal(live.ctx);
                }
                live.put(kept.as_ref())?;
                continue;
            }
            Phase::Undoing if current == journal.original => {
                let kept = match &journal.from {
                    Some(from) => live.kept(from)?,
                    None => None,
                };
                if let Some(k) = &kept {
                    live.name(Some(&k.oauth_account))?;
                }
                journal.phase = Phase::Undone;
            }
            Phase::SetAside | Phase::Undoing => {
                // The login in use is neither the one before nor the one put in place: a /login
                // since, or tokens refreshed. Leave it, and forget what might be stale.
                stale(live, &journal)?;
                return remove_journal(live.ctx);
            }
            Phase::Committed => {
                if journal.target.is_some() {
                    if let Some(to) = &journal.to {
                        live.forget(to).context("forgetting the sign-in put back")?;
                    }
                }
                return remove_journal(live.ctx);
            }
            Phase::Undone => {
                // The login from before is in use again: its copy goes.
                if let Some(from) = &journal.from {
                    live.forget(from)?;
                }
                return remove_journal(live.ctx);
            }
        }
        live.save(&journal)?;
    }
}

/// Forget every copy the interrupted switch touched: either may be out of date now.
fn stale(live: &Live, journal: &Journal) -> Result<()> {
    live.ctx.log("an interrupted switch of the command line could not be settled for sure: its copies are forgotten, and the accounts sign in again with /login");
    for account in [&journal.from, &journal.to].into_iter().flatten() {
        live.forget(account)?;
    }
    Ok(())
}

fn journal_path(ctx: &Ctx) -> PathBuf {
    ctx.paths.state_dir.join(JOURNAL)
}

fn read_journal(ctx: &Ctx) -> Result<Option<Journal>> {
    match fs::read(journal_path(ctx)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context("reading the command line's switch journal")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn remove_journal(ctx: &Ctx) -> Result<()> {
    match fs::remove_file(journal_path(ctx)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The account's part of Claude Code's item: everything but the machine's shared fields.
fn own_part(credentials: &Map<String, Value>) -> Map<String, Value> {
    credentials
        .iter()
        .filter(|(k, _)| !SHARED_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn shared_part(credentials: &Map<String, Value>) -> Map<String, Value> {
    credentials.iter().filter(|(k, _)| SHARED_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// SHA-256 of the account's part of a login, with its keys in order: the same login gives the
/// same fingerprint however it was written.
fn fingerprint(credentials: &Map<String, Value>) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&o[k]))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    let canonical = sorted(&Value::Object(own_part(credentials))).to_string();
    Sha256::digest(canonical.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// JSON with every character beyond ASCII escaped: the keychain tool prints anything else as hex.
fn ascii_json(value: &Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            // Outside strings JSON has only ASCII, so every such character is inside one.
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn claude_home(ctx: &Ctx) -> PathBuf {
    ctx.paths.claude_settings.parent().map(Path::to_path_buf).unwrap_or_else(|| crate::paths::home().join(".claude"))
}

/// The name Claude Code files its keychain item under.
fn user_name() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        #[cfg(unix)]
        {
            // SAFETY: getpwuid returns a pointer into static storage, or null.
            unsafe {
                let pw = libc::getpwuid(libc::geteuid());
                if !pw.is_null() && !(*pw).pw_name.is_null() {
                    return std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
                }
            }
        }
        "claude-code-user".into()
    })
}

fn read_config(path: &Path) -> Result<Map<String, Value>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `~/.claude.json` the way Claude Code writes it: two-space indents.
fn write_config(path: &Path, config: &Map<String, Value>) -> Result<()> {
    let body = serde_json::to_vec_pretty(config)?;
    fsx::atomic_write(path, &body, None, false).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `<path>.lock`, where Claude Code's lock library keeps the lock for `path`.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// The three locks a switch holds, in Claude Code's order.
struct Locks(Vec<DirLock>);

impl Locks {
    fn take(ctx: &Ctx) -> Result<Locks> {
        let home = claude_home(ctx);
        Ok(Locks(vec![
            DirLock::acquire(&home.join(".oauth_refresh.lock"), Duration::from_secs(60))?,
            DirLock::acquire(&lock_path(&home), Duration::from_secs(60))?,
            DirLock::acquire(&lock_path(&ctx.paths.claude_json), Duration::from_secs(10))?,
        ]))
    }

    /// Whether every lock is still ours.
    fn check(&self) -> Result<()> {
        self.0.iter().try_for_each(DirLock::check)
    }
}

/// One of Claude Code's locks: a folder, made to take it and removed to give it back. While held,
/// its time is refreshed every few seconds, as Claude Code's lock library does, so nobody takes it
/// for one left behind; one not refreshed for longer than `stale` was left behind, and is taken.
/// A lock whose time is not the one last set was taken over by someone else: it is no longer ours,
/// is not refreshed or removed, and [`DirLock::check`] says so.
struct DirLock {
    path: PathBuf,
    state: Arc<LockState>,
    keeper: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct LockState {
    stop: AtomicBool,
    lost: AtomicBool,
    /// The time last set on the folder, as read back.
    stamp: Mutex<Option<SystemTime>>,
}

impl LockState {
    /// Whether the folder still has the time last set on it; marks the lock lost when it does not.
    fn ours(&self, path: &Path) -> bool {
        if self.lost.load(Ordering::Relaxed) {
            return false;
        }
        // Read under the same lock a refresh holds, so a refresh never seems a takeover.
        let stamp = self.stamp.lock().unwrap_or_else(|e| e.into_inner());
        let now = fs::metadata(path).and_then(|m| m.modified()).ok();
        let ours = now.is_some() && now == *stamp;
        drop(stamp);
        if !ours {
            self.lost.store(true, Ordering::Relaxed);
        }
        ours
    }

    fn refresh(&self, path: &Path) {
        let mut stamp = self.stamp.lock().unwrap_or_else(|e| e.into_inner());
        let _ = fs::File::open(path).and_then(|f| f.set_modified(SystemTime::now()));
        *stamp = fs::metadata(path).and_then(|m| m.modified()).ok();
    }
}

impl DirLock {
    const WAIT: Duration = Duration::from_secs(10);
    const REFRESH: Duration = Duration::from_secs(2);

    fn acquire(path: &Path, stale: Duration) -> Result<DirLock> {
        Self::acquire_within(path, stale, Self::WAIT)
    }

    fn acquire_within(path: &Path, stale: Duration, wait: Duration) -> Result<DirLock> {
        let deadline = Instant::now() + wait;
        loop {
            match fs::create_dir(path) {
                Ok(()) => return Ok(Self::held(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > stale) && fs::remove_dir(path).is_ok() {
                        continue;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Its folder is missing: nothing of Claude Code's to guard there.
                    return Ok(DirLock { path: PathBuf::new(), state: Arc::default(), keeper: None });
                }
                Err(e) => return Err(e).with_context(|| format!("taking {}", path.display())),
            }
            if Instant::now() >= deadline {
                bail!("Claude Code is busy with its sign-in ({} is taken); try again in a moment", path.display());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn held(path: &Path) -> DirLock {
        let state = Arc::new(LockState::default());
        *state.stamp.lock().unwrap_or_else(|e| e.into_inner()) = fs::metadata(path).and_then(|m| m.modified()).ok();
        let keeper = {
            let (path, state) = (path.to_path_buf(), state.clone());
            std::thread::spawn(move || {
                let mut last = Instant::now();
                while !state.stop.load(Ordering::Relaxed) {
                    if last.elapsed() >= Self::REFRESH {
                        if !state.ours(&path) {
                            return;
                        }
                        state.refresh(&path);
                        last = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        DirLock { path: path.to_path_buf(), state, keeper: Some(keeper) }
    }

    fn check(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() || self.state.ours(&self.path) {
            Ok(())
        } else {
            bail!(
                "{} was taken over while switching the command line; the switch is settled next time",
                self.path.display()
            )
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
        if !self.path.as_os_str().is_empty() && self.state.ours(&self.path) {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{Folder, Secrets};
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;

    const ADA: &str = "5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d";
    const GRACE: &str = "9e31ea7e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";
    const CY: &str = "c7c7c7c7-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

    /// The keychain folder, which can be made to fail its reads.
    struct Flaky {
        inner: Folder,
        broken: Rc<Cell<bool>>,
    }

    impl Secrets for Flaky {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
            if self.broken.get() {
                bail!("the keychain is locked");
            }
            self.inner.get(service, account)
        }
        fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
            self.inner.set(service, account, value)
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.inner.delete(service, account)
        }
    }

    struct Mac {
        #[cfg_attr(not(unix), allow(dead_code))]
        tmp: tempfile::TempDir,
        ctx: Ctx,
        stores: Stores,
        keychain: Folder,
        broken: Rc<Cell<bool>>,
        locks: Locks,
    }

    impl Mac {
        fn new() -> Mac {
            let tmp = tempfile::tempdir().unwrap();
            let mut paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
            paths.claude_json = tmp.path().join(".claude.json");
            paths.claude_settings = tmp.path().join(".claude/settings.json");
            paths.keychain_dir = Some(tmp.path().join("keychain"));
            fs::create_dir_all(tmp.path().join(".claude")).unwrap();
            let fake = FakeDesktop { running: Some(false), active: None };
            let ctx = Ctx::new(paths, Config::default(), fake, LogSink::Silent);
            let broken = Rc::new(Cell::new(false));
            let dir = tmp.path().join("keychain");
            let stores = Stores {
                live: Box::new(Flaky { inner: Folder(dir.clone()), broken: broken.clone() }),
                kept: Box::new(Folder(dir.clone())),
            };
            Mac { keychain: Folder(dir), tmp, ctx, stores, broken, locks: Locks(Vec::new()) }
        }

        fn live(&self) -> Live<'_> {
            Live { ctx: &self.ctx, stores: &self.stores, user: user_name(), locks: &self.locks }
        }

        /// What `/login` in claude does: a fresh login in the keychain next to the shared fields
        /// there, who it belongs to in the config.
        fn login(&self, account: &str) {
            self.login_as(account, &format!("refresh of {account}"));
        }

        fn login_as(&self, account: &str, refresh: &str) {
            let mut item = self.item().unwrap_or_default();
            item.insert("claudeAiOauth".into(), json!({ "refreshToken": refresh }));
            item.insert("trustedDeviceToken".into(), json!(format!("device of {account}")));
            self.set_item(&item);
            let mut config = read_config(&self.ctx.paths.claude_json).unwrap();
            if config.is_empty() {
                config.insert("numStartups".into(), 7.into());
                config.insert("projects".into(), json!({"/tmp/p": {"allowedTools": []}}));
            }
            let email = format!("{}@example.com", &account[..4]);
            let oauth = json!({ "accountUuid": account, "emailAddress": email, "displayName": "José" });
            config.insert("oauthAccount".into(), oauth);
            write_config(&self.ctx.paths.claude_json, &config).unwrap();
        }

        /// What a running claude does when its tokens rotate.
        fn rotate(&self, refresh: &str) {
            let mut item = self.item().unwrap();
            item.insert("claudeAiOauth".into(), json!({ "refreshToken": refresh }));
            self.set_item(&item);
        }

        fn item(&self) -> Option<Map<String, Value>> {
            let text = self.keychain.get(LIVE_SERVICE, &user_name()).unwrap()?;
            Some(serde_json::from_str::<Value>(&text).unwrap().as_object().unwrap().clone())
        }

        fn set_item(&self, item: &Map<String, Value>) {
            self.keychain.set(LIVE_SERVICE, &user_name(), &Value::Object(item.clone()).to_string()).unwrap();
        }

        /// The refresh token in use, and who the config says it belongs to.
        fn on(&self) -> Option<(String, String)> {
            let config = read_config(&self.ctx.paths.claude_json).unwrap();
            let who = config.get("oauthAccount").map(|o| o["accountUuid"].as_str().unwrap().to_string());
            let refresh = self
                .item()
                .and_then(|i| i.get("claudeAiOauth").map(|o| o["refreshToken"].as_str().unwrap().to_string()));
            match (who, refresh) {
                (Some(who), Some(refresh)) => Some((who, refresh)),
                (None, None) => None,
                (who, refresh) => panic!("signed in halfway: {who:?} / {refresh:?}"),
            }
        }

        fn kept(&self, account: &str) -> Option<String> {
            let text = self.keychain.get(KEPT_SERVICE, account).unwrap()?;
            let kept: Kept = serde_json::from_str(&text).unwrap();
            Some(kept.credentials["claudeAiOauth"]["refreshToken"].as_str().unwrap().to_string())
        }

        fn follow(&self, to: Option<&str>) -> Result<Followed> {
            follow_with(&self.live(), to)
        }

        fn journal(&self) -> Option<Journal> {
            read_journal(&self.ctx).unwrap()
        }

        fn oauth_account(&self) -> Map<String, Value> {
            read_config(&self.ctx.paths.claude_json).unwrap()["oauthAccount"].as_object().unwrap().clone()
        }

        /// Interrupt a switch from the one in use to `to` at `phase`, having put `to`'s login in
        /// place when `put` says so.
        fn interrupt(&self, to: &str, phase: Phase, put: bool) {
            let live = self.live();
            let from = self.on().unwrap().0;
            let target = live.kept(to).unwrap();
            let journal = Journal {
                from: Some(from.clone()),
                to: Some(to.into()),
                phase,
                original: live.current().unwrap(),
                target: target.as_ref().map(|k| fingerprint(&k.credentials)),
            };
            live.save(&journal).unwrap();
            live.keep(&from, &self.item().unwrap(), self.oauth_account()).unwrap();
            if put {
                let mut item = self.item().unwrap();
                item.extend(own_part(&target.unwrap().credentials));
                self.set_item(&item);
            }
        }
    }

    fn on(account: &str, refresh: &str) -> Option<(String, String)> {
        Some((account.into(), refresh.into()))
    }

    /// Ada and Grace both signed in once, through cc-same; Ada in use, Grace kept.
    fn both() -> Mac {
        let mac = Mac::new();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        mac.login(ADA);
        mac
    }

    #[test]
    fn the_command_line_follows_from_its_second_switch_on() {
        let mac = Mac::new();
        mac.login(ADA);
        // Nothing kept for Grace yet: signed out, Ada's sign-in kept, until /login to Grace.
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::SignedOut { from: ADA.into() });
        assert_eq!(mac.on(), None);
        assert!(mac.kept(ADA).is_some());
        mac.login(GRACE);
        // From now on, both switch.
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::Switched { from: Some(GRACE.into()) });
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(ADA).is_none() && mac.kept(GRACE).is_some());
        // Tokens rotate while claude runs: switching away keeps the newest.
        mac.rotate("rotated ada");
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::Switched { from: Some(ADA.into()) });
        assert_eq!(mac.on(), on(GRACE, &format!("refresh of {GRACE}")));
        assert_eq!(mac.kept(ADA).as_deref(), Some("rotated ada"));
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::AlreadyThere);
        assert!(mac.journal().is_none());
    }

    /// The sign-ins of MCP servers belong to the machine: what is in use stays in use, and one
    /// gone stays gone.
    #[test]
    fn the_machines_shared_sign_ins_are_never_rolled_back() {
        let mac = Mac::new();
        let with_mcp = |token: Option<&str>| {
            let mut item = mac.item().unwrap_or_default();
            match token {
                Some(t) => item.insert("mcpOAuth".into(), json!({ "server": t })),
                None => item.remove("mcpOAuth"),
            };
            mac.set_item(&item);
        };
        with_mcp(Some("R1"));
        mac.login(ADA);
        mac.follow(Some(GRACE)).unwrap();
        assert_eq!(mac.item().unwrap()["mcpOAuth"]["server"], "R1");
        mac.login(GRACE);
        with_mcp(Some("R2"));
        mac.follow(Some(ADA)).unwrap();
        assert_eq!(mac.item().unwrap()["mcpOAuth"]["server"], "R2");
        with_mcp(None);
        mac.follow(Some(GRACE)).unwrap();
        assert!(!mac.item().unwrap().contains_key("mcpOAuth"));
    }

    #[test]
    fn everything_else_in_the_config_stays() {
        let mac = Mac::new();
        mac.login(ADA);
        let before = read_config(&mac.ctx.paths.claude_json).unwrap();
        mac.follow(Some(GRACE)).unwrap();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        assert_eq!(read_config(&mac.ctx.paths.claude_json).unwrap(), before);
    }

    #[test]
    fn kept_copies_are_plain_ascii() {
        let mac = Mac::new();
        mac.login(ADA);
        mac.follow(Some(GRACE)).unwrap();
        let text = mac.keychain.get(KEPT_SERVICE, ADA).unwrap().unwrap();
        assert!(text.is_ascii() && text.contains("Jos\\u00e9"), "{text}");
    }

    #[test]
    fn signed_out_by_a_switch_signs_back_in_by_switching_back() {
        let mac = Mac::new();
        mac.login(ADA);
        mac.follow(Some(GRACE)).unwrap();
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::Switched { from: None });
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(ADA).is_none());
    }

    #[test]
    fn signing_out_to_add_an_account_leaves_the_command_line_be() {
        let mac = Mac::new();
        mac.login(ADA);
        assert_eq!(mac.follow(None).unwrap(), Followed::Stayed { on: ADA.into() });
        assert!(mac.kept(ADA).is_none());
    }

    #[test]
    fn not_signed_in_means_nothing_to_do() {
        let mac = Mac::new();
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::NotSignedIn);
        assert!(mac.journal().is_none());
    }

    #[test]
    fn a_damaged_copy_changes_nothing_and_says_nothing_of_it() {
        let mac = Mac::new();
        mac.login(ADA);
        let secret = r#""{\"credentials\":{\"claudeAiOauth\":{\"refreshToken\":\"sk-ant-ort-SECRET\"}}}""#;
        mac.keychain.set(KEPT_SERVICE, GRACE, secret).unwrap();
        let e = mac.follow(Some(GRACE)).unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"), "{e:#}");
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(ADA).is_none() && mac.journal().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_switch_puts_the_sign_in_back() {
        let mac = both();
        // ~/.claude.json cannot be written (a symlink is never written through), so the switch
        // fails after Grace's login went in: it must not stay there.
        let config = &mac.ctx.paths.claude_json;
        let real = mac.tmp.path().join("claude.json.real");
        fs::rename(config, &real).unwrap();
        std::os::unix::fs::symlink(&real, config).unwrap();
        assert!(mac.follow(Some(GRACE)).is_err());
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(GRACE).is_some() && mac.kept(ADA).is_none());
        assert!(mac.journal().is_none());
    }

    // ------------------------------------------------------------ interrupted switches

    /// Set aside, then interrupted before anything was put in place: undone.
    #[test]
    fn interrupted_before_anything_was_put_in_place() {
        let mac = both();
        mac.interrupt(GRACE, Phase::SetAside, false);
        mac.follow(None).unwrap();
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(ADA).is_none() && mac.kept(GRACE).is_some() && mac.journal().is_none());
    }

    /// Interrupted with Grace's login in place, unused since: the switch is finished.
    #[test]
    fn interrupted_with_the_new_login_in_place() {
        let mac = both();
        mac.interrupt(GRACE, Phase::SetAside, true);
        mac.follow(None).unwrap();
        assert_eq!(mac.on(), on(GRACE, &format!("refresh of {GRACE}")));
        assert_eq!(mac.kept(ADA), Some(format!("refresh of {ADA}")));
        assert!(mac.kept(GRACE).is_none() && mac.journal().is_none());
    }

    /// Interrupted, then Ada signed in afresh with /login: her new login is never overwritten
    /// with the copy from before.
    #[test]
    fn a_newer_login_to_the_same_account_is_never_overwritten() {
        let mac = both();
        mac.interrupt(GRACE, Phase::SetAside, false);
        mac.login_as(ADA, "ada after /login");
        mac.follow(None).unwrap();
        assert_eq!(mac.on(), on(ADA, "ada after /login"));
        assert!(mac.kept(ADA).is_none() && mac.journal().is_none());
        // What might have been stale is forgotten too.
        assert!(mac.kept(GRACE).is_none());
    }

    /// Interrupted while Ada was being set aside; Ada's tokens rotated; then /login to Cy. A
    /// switch to Ada must not put back the copy made before the rotation.
    #[test]
    fn a_copy_made_before_a_rotation_is_never_put_back() {
        let mac = both();
        mac.interrupt(GRACE, Phase::Started, false);
        mac.rotate("rotated ada");
        mac.login(CY);
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::SignedOut { from: CY.into() });
        assert_eq!(mac.on(), None);
    }

    /// Interrupted with Grace's login in place; Grace's tokens rotated; then /login to Cy. A
    /// switch to Grace must not put back the copy that was already used.
    #[test]
    fn a_copy_that_was_used_is_never_put_back() {
        let mac = both();
        mac.interrupt(GRACE, Phase::SetAside, true);
        mac.rotate("rotated grace");
        mac.login(CY);
        mac.follow(None).unwrap();
        assert!(mac.kept(GRACE).is_none() && mac.kept(ADA).is_none());
        assert_eq!(mac.on(), on(CY, &format!("refresh of {CY}")));
    }

    /// An undo interrupted after the login from before was back: it finishes, no "gone" error.
    #[test]
    fn an_interrupted_undo_finishes() {
        let mac = both();
        mac.interrupt(GRACE, Phase::SetAside, true);
        let mut journal = mac.journal().unwrap();
        journal.phase = Phase::Undoing;
        mac.live().save(&journal).unwrap();
        // Put back, and its copy forgotten, before the journal said so.
        let ada = mac.live().kept(ADA).unwrap().unwrap();
        mac.live().put(Some(&ada)).unwrap();
        mac.keychain.delete(KEPT_SERVICE, ADA).unwrap();
        mac.follow(None).unwrap();
        assert_eq!(mac.on(), on(ADA, &format!("refresh of {ADA}")));
        assert!(mac.kept(GRACE).is_some() && mac.journal().is_none());
    }

    /// The keychain cannot be read while settling: nothing changes, the shared fields included,
    /// and the journal stays for next time.
    #[test]
    fn a_keychain_that_cannot_be_read_stops_the_settling() {
        let mac = both();
        let mut item = mac.item().unwrap();
        item.insert("mcpOAuth".into(), json!({ "server": "R1" }));
        mac.set_item(&item);
        mac.interrupt(GRACE, Phase::SetAside, true);
        mac.broken.set(true);
        assert!(mac.follow(None).is_err());
        mac.broken.set(false);
        assert_eq!(mac.item().unwrap()["mcpOAuth"]["server"], "R1");
        assert!(mac.journal().is_some());
        mac.follow(None).unwrap();
        assert_eq!(mac.item().unwrap()["mcpOAuth"]["server"], "R1");
        assert!(mac.journal().is_none());
    }

    // ------------------------------------------------------------ locks

    #[cfg(unix)]
    #[test]
    fn claude_codes_locks_are_waited_for_kept_fresh_and_given_back() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join(".oauth_refresh.lock");
        // Held by a running claude: given up on.
        fs::create_dir(&lock).unwrap();
        assert!(DirLock::acquire_within(&lock, Duration::from_secs(60), Duration::from_millis(300)).is_err());
        // Left behind long ago: taken over, and given back.
        drop(DirLock::acquire_within(&lock, Duration::ZERO, Duration::from_millis(300)).unwrap());
        assert!(!lock.exists());
        // Kept fresh while held.
        let held = DirLock::acquire(&lock, Duration::from_secs(60)).unwrap();
        std::thread::sleep(DirLock::REFRESH + Duration::from_millis(500));
        let age = SystemTime::now().duration_since(fs::metadata(&lock).unwrap().modified().unwrap()).unwrap();
        assert!(age < DirLock::REFRESH, "{age:?}");
        assert!(held.check().is_ok());
        drop(held);
        assert!(!lock.exists());
    }

    /// Taken over by someone else meanwhile: the switch is told, and their lock is left alone.
    #[cfg(unix)]
    #[test]
    fn a_lock_taken_over_is_noticed_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("x.lock");
        let held = DirLock::acquire(&lock, Duration::from_secs(60)).unwrap();
        // Someone found it stale, removed it, and made their own.
        fs::remove_dir(&lock).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::create_dir(&lock).unwrap();
        fs::File::open(&lock).unwrap().set_modified(SystemTime::now() + Duration::from_secs(1)).unwrap();
        assert!(held.check().is_err());
        drop(held);
        assert!(lock.exists(), "their lock was removed");
    }

    #[test]
    fn a_lock_that_cannot_be_cleared_is_given_up_on_in_time() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("x.lock");
        fs::create_dir(&lock).unwrap();
        fs::write(lock.join("inside"), "").unwrap();
        let started = Instant::now();
        assert!(DirLock::acquire_within(&lock, Duration::ZERO, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn fingerprints_ignore_key_order_and_shared_fields() {
        let a = json!({"claudeAiOauth": {"a": 1, "b": 2}, "mcpOAuth": {"x": 1}});
        let b = json!({"claudeAiOauth": {"b": 2, "a": 1}});
        assert_eq!(fingerprint(a.as_object().unwrap()), fingerprint(b.as_object().unwrap()));
        let c = json!({"claudeAiOauth": {"a": 1, "b": 3}});
        assert_ne!(fingerprint(a.as_object().unwrap()), fingerprint(c.as_object().unwrap()));
    }

    #[test]
    fn the_real_keychain_is_never_used_with_a_stand_in_claude() {
        let mac = Mac::new();
        let mut paths = mac.ctx.paths.clone();
        paths.keychain_dir = None;
        let fake = FakeDesktop { running: Some(false), active: None };
        let ctx = Ctx::new(paths, Config::default(), fake, LogSink::Silent);
        assert!(secrets::open(&ctx).is_err());
    }
}
