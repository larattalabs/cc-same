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
//! * Tokens rotate as they are used, so a copy put back is forgotten before the switch counts as
//!   done: it is set aside afresh when it is left.
//! * A journal (no secrets in it: who from, who to, how far) makes a switch all or nothing. One
//!   that was interrupted is undone, or finished, before anything else is done.
//! * Claude Code refreshes its tokens under two lock folders, and writes `~/.claude.json` under a
//!   third. A switch holds all three, keeping them fresh while it does, so a `claude` running
//!   meanwhile finishes a refresh first, or finds the new account's tokens after. Only
//!   `oauthAccount` in `~/.claude.json` changes; the rest keeps its content (not its spacing).
//! * Only the default configuration folder: with `CLAUDE_CONFIG_DIR` set, Claude Code names its
//!   keychain item after that folder, and the command line is left alone.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::is_uuid;
use crate::secrets::{self, Stores};
use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    /// Nothing changed yet.
    #[default]
    Started,
    /// The sign-in in use is kept; the switch may have changed the live one since.
    SetAside,
    /// Everything is in place; only forgetting the copy put back is left.
    Committed,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Journal {
    from: Option<String>,
    to: Option<String>,
    phase: Phase,
}

/// Bring the command line to `to` (or leave it be, for `None`).
pub fn follow(ctx: &Ctx, to: Option<&str>) -> Result<Followed> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        bail!("CLAUDE_CONFIG_DIR is set, so Claude Code keeps its sign-in elsewhere; the command line was left alone");
    }
    let stores = secrets::open(ctx)?;
    follow_with(ctx, &stores, to)
}

/// Forget the command-line sign-in kept for `account`, wherever switching is possible at all.
pub fn forget(ctx: &Ctx, account: &str) -> Result<()> {
    match secrets::open(ctx) {
        Ok(stores) => stores.kept.delete(KEPT_SERVICE, account),
        // No keychain here: nothing can have been kept.
        Err(_) => Ok(()),
    }
}

struct Live<'a> {
    ctx: &'a Ctx,
    stores: &'a Stores,
    user: String,
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
        let kept = Kept { credentials: own_part(credentials), oauth_account, set_aside_at: fsx::now_secs() };
        self.stores.kept.set(KEPT_SERVICE, account, &ascii_json(&serde_json::to_value(kept)?))
    }

    /// Put `kept` in place (nobody, for `None`), next to the machine's shared fields from `shared`.
    fn put(&self, kept: Option<&Kept>, shared: &Map<String, Value>) -> Result<()> {
        let mut credentials: Map<String, Value> = kept.map(|k| own_part(&k.credentials)).unwrap_or_default();
        credentials.extend(shared_part(shared));
        if credentials.is_empty() {
            self.stores.live.delete(LIVE_SERVICE, &self.user)?;
        } else {
            self.stores.live.set(LIVE_SERVICE, &self.user, &ascii_json(&Value::Object(credentials)))?;
        }
        let mut config = self.config()?;
        let wanted = kept.map(|k| Value::Object(k.oauth_account.clone()));
        if config.get("oauthAccount") == wanted.as_ref() {
            // Already so (an undo after the config could not be written): nothing to write.
            return Ok(());
        }
        match wanted {
            Some(oauth_account) => config.insert("oauthAccount".into(), oauth_account),
            None => config.remove("oauthAccount"),
        };
        write_config(&self.ctx.paths.claude_json, &config)
    }

    /// Who `~/.claude.json` says the command line is signed in to.
    fn named(&self) -> Result<Option<String>> {
        Ok(self
            .config()?
            .get("oauthAccount")
            .and_then(|o| o.get("accountUuid"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }
}

fn follow_with(ctx: &Ctx, stores: &Stores, to: Option<&str>) -> Result<Followed> {
    let home = claude_home(ctx);
    let _refresh = DirLock::acquire(&home.join(".oauth_refresh.lock"), Duration::from_secs(60))?;
    let _legacy = DirLock::acquire(&lock_path(&home), Duration::from_secs(60))?;
    let _config = DirLock::acquire(&lock_path(&ctx.paths.claude_json), Duration::from_secs(10))?;
    let live = Live { ctx, stores, user: user_name() };
    recover_with(&live)?;

    let config = live.config()?;
    let credentials = live.credentials()?;
    let shared = credentials.clone().unwrap_or_default();
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
        // A copy kept earlier is out of date now.
        stores.kept.delete(KEPT_SERVICE, to)?;
        return Ok(Followed::AlreadyThere);
    }
    let target = live.kept(to)?;
    if on.is_none() && target.is_none() {
        return Ok(Followed::NotSignedIn);
    }

    let mut journal = Journal { from: on.clone(), to: Some(to.to_string()), phase: Phase::Started };
    save_journal(ctx, &journal)?;
    if let (Some(from), Some(credentials)) = (&on, &credentials) {
        let oauth_account = config.get("oauthAccount").and_then(Value::as_object).cloned().unwrap_or_default();
        live.keep(from, credentials, oauth_account)?;
    }
    journal.phase = Phase::SetAside;
    save_journal(ctx, &journal)?;
    if let Err(e) = live.put(target.as_ref(), &shared) {
        // Undo now; if that fails too, the journal undoes it the next time.
        undo(&live, &journal).context("putting the command line's sign-in back")?;
        return Err(e);
    }
    journal.phase = Phase::Committed;
    save_journal(ctx, &journal)?;
    finish(&live, &journal)?;
    Ok(match (target, on) {
        (Some(_), from) => Followed::Switched { from },
        (None, Some(from)) => Followed::SignedOut { from },
        (None, None) => unreachable!("returned above"),
    })
}

/// Finish or undo a switch that was interrupted.
fn recover_with(live: &Live) -> Result<()> {
    let Some(journal) = read_journal(live.ctx)? else { return Ok(()) };
    match journal.phase {
        Phase::Started => remove_journal(live.ctx),
        Phase::SetAside => undo(live, &journal),
        Phase::Committed => finish(live, &journal),
    }
}

/// Put back the sign-in set aside, and forget its copy: it is in use again.
fn undo(live: &Live, journal: &Journal) -> Result<()> {
    let on = live.named()?;
    // Someone signed in since (with /login): that sign-in stays, and what was set aside stays kept.
    let ours = on.is_none() || on == journal.from || on == journal.to;
    if ours {
        // A keychain that cannot be read stops the undo: its shared fields must not be lost.
        let shared = live.credentials()?.unwrap_or_default();
        match &journal.from {
            Some(from) => {
                let kept = live.kept(from)?.ok_or_else(|| anyhow!("the sign-in set aside for {from} is gone"))?;
                live.put(Some(&kept), &shared)?;
                live.stores.kept.delete(KEPT_SERVICE, from)?;
            }
            None => live.put(None, &shared)?,
        }
    }
    remove_journal(live.ctx)
}

/// Forget the copy put back: from now on it is the one in use.
fn finish(live: &Live, journal: &Journal) -> Result<()> {
    if let Some(to) = &journal.to {
        live.stores.kept.delete(KEPT_SERVICE, to).context("forgetting the sign-in put back")?;
    }
    remove_journal(live.ctx)
}

fn journal_path(ctx: &Ctx) -> PathBuf {
    ctx.paths.state_dir.join(JOURNAL)
}

fn save_journal(ctx: &Ctx, journal: &Journal) -> Result<()> {
    fsx::create_private_dir_all(&ctx.paths.state_dir)?;
    fsx::atomic_write(&journal_path(ctx), &serde_json::to_vec_pretty(journal)?, None, false)?;
    Ok(())
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

/// One of Claude Code's locks: a folder, made to take it and removed to give it back, its time
/// refreshed every few seconds while held, as Claude Code's lock library does, so nobody takes it
/// for one left behind. One not refreshed for longer than `stale` was left behind, and is taken.
struct DirLock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    keeper: Option<std::thread::JoinHandle<()>>,
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
                    return Ok(DirLock { path: PathBuf::new(), stop: Arc::default(), keeper: None });
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
        let stop = Arc::new(AtomicBool::new(false));
        let keeper = {
            let (path, stop) = (path.to_path_buf(), stop.clone());
            std::thread::spawn(move || {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if last.elapsed() >= Self::REFRESH {
                        touch(&path);
                        last = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        DirLock { path: path.to_path_buf(), stop, keeper: Some(keeper) }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

/// Set a folder's time to now.
fn touch(path: &Path) {
    let _ = fs::File::open(path).and_then(|f| f.set_modified(SystemTime::now()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{Folder, Secrets as _};
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use serde_json::json;

    const ADA: &str = "5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d";
    const GRACE: &str = "9e31ea7e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

    struct Mac {
        #[cfg_attr(not(unix), allow(dead_code))]
        tmp: tempfile::TempDir,
        ctx: Ctx,
        stores: Stores,
        keychain: Folder,
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
            let stores = secrets::open(&ctx).unwrap();
            Mac { keychain: Folder(tmp.path().join("keychain")), tmp, ctx, stores }
        }

        /// What `/login` in claude does: its login in the keychain next to the shared fields there,
        /// who it belongs to in the config.
        fn login(&self, account: &str) {
            let mut item = self.item().unwrap_or_default();
            item.insert("claudeAiOauth".into(), json!({ "refreshToken": format!("refresh of {account}") }));
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

        fn item(&self) -> Option<Map<String, Value>> {
            let text = self.keychain.get(LIVE_SERVICE, &user_name()).unwrap()?;
            Some(serde_json::from_str::<Value>(&text).unwrap().as_object().unwrap().clone())
        }

        fn set_item(&self, item: &Map<String, Value>) {
            self.keychain.set(LIVE_SERVICE, &user_name(), &Value::Object(item.clone()).to_string()).unwrap();
        }

        /// Who the command line is signed in to, by its config and by the login in its item.
        fn on(&self) -> Option<String> {
            let config = read_config(&self.ctx.paths.claude_json).unwrap();
            let who = config.get("oauthAccount").map(|o| o["accountUuid"].as_str().unwrap().to_string());
            let login = self.item().and_then(|i| i.get("claudeAiOauth").cloned());
            match (&who, login) {
                (Some(who), Some(login)) => assert_eq!(login["refreshToken"], format!("refresh of {who}")),
                (None, None) => {}
                (who, login) => panic!("signed in halfway: {who:?} / {login:?}"),
            }
            who
        }

        fn kept(&self, account: &str) -> Option<Kept> {
            self.keychain.get(KEPT_SERVICE, account).unwrap().map(|k| serde_json::from_str(&k).unwrap())
        }

        fn follow(&self, to: Option<&str>) -> Result<Followed> {
            follow_with(&self.ctx, &self.stores, to)
        }

        fn journal(&self) -> Option<Journal> {
            read_journal(&self.ctx).unwrap()
        }

        fn live(&self) -> Live<'_> {
            Live { ctx: &self.ctx, stores: &self.stores, user: user_name() }
        }

        fn oauth_account(&self) -> Map<String, Value> {
            read_config(&self.ctx.paths.claude_json).unwrap()["oauthAccount"].as_object().unwrap().clone()
        }
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
        assert_eq!(mac.on().as_deref(), Some(ADA));
        assert!(mac.kept(ADA).is_none() && mac.kept(GRACE).is_some());
        // Tokens rotate while claude runs: switching away keeps the newest.
        let mut item = mac.item().unwrap();
        item.insert("claudeAiOauth".into(), json!({ "refreshToken": format!("refresh of {ADA}"), "rotated": true }));
        mac.set_item(&item);
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::Switched { from: Some(ADA.into()) });
        assert_eq!(mac.on().as_deref(), Some(GRACE));
        let ada = mac.kept(ADA).unwrap();
        assert_eq!(ada.credentials["claudeAiOauth"]["rotated"], true);
        assert_eq!(ada.credentials["trustedDeviceToken"], format!("device of {ADA}"));
        assert_eq!(ada.oauth_account["displayName"], "José");
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
        // Signed out, the shared field stays; and it is not kept with Ada's sign-in.
        assert_eq!(mac.item().unwrap()["mcpOAuth"]["server"], "R1");
        assert!(!mac.kept(ADA).unwrap().credentials.contains_key("mcpOAuth"));
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

    /// Kept copies are ASCII, which the keychain tool prints as they are.
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
        assert_eq!(mac.on(), None);
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::Switched { from: None });
        assert_eq!(mac.on().as_deref(), Some(ADA));
        assert!(mac.kept(ADA).is_none());
    }

    #[test]
    fn signing_out_to_add_an_account_leaves_the_command_line_be() {
        let mac = Mac::new();
        mac.login(ADA);
        assert_eq!(mac.follow(None).unwrap(), Followed::Stayed { on: ADA.into() });
        assert_eq!(mac.on().as_deref(), Some(ADA));
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
        // A copy that went in twice encoded: a JSON string holding the JSON.
        let secret = r#""{\"credentials\":{\"claudeAiOauth\":{\"refreshToken\":\"sk-ant-ort-SECRET\"}}}""#;
        mac.keychain.set(KEPT_SERVICE, GRACE, secret).unwrap();
        let e = mac.follow(Some(GRACE)).unwrap_err();
        assert!(!format!("{e:#}").contains("SECRET"), "{e:#}");
        assert_eq!(mac.on().as_deref(), Some(ADA));
        assert!(mac.kept(ADA).is_none() && mac.journal().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_switch_puts_the_sign_in_back() {
        let mac = Mac::new();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        mac.login(ADA);
        // ~/.claude.json cannot be written (a symlink is never written through), so the switch
        // fails after Grace's login went in: it must not stay there.
        let config = &mac.ctx.paths.claude_json;
        let real = mac.tmp.path().join("claude.json.real");
        fs::rename(config, &real).unwrap();
        std::os::unix::fs::symlink(&real, config).unwrap();
        assert!(mac.follow(Some(GRACE)).is_err());
        assert_eq!(mac.on().as_deref(), Some(ADA));
        // Grace's copy is still kept, to try again; Ada's is in use, so it is not.
        assert!(mac.kept(GRACE).is_some() && mac.kept(ADA).is_none());
        assert!(mac.journal().is_none());
    }

    /// Interrupted with the live sign-in half changed: the next switch undoes it first.
    #[test]
    fn an_interrupted_switch_is_undone_before_anything_else() {
        let mac = Mac::new();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        mac.login(ADA);
        // Ada set aside, Grace's login put in the keychain, then the power went out.
        mac.live().keep(ADA, &mac.item().unwrap(), mac.oauth_account()).unwrap();
        let mut item = mac.item().unwrap();
        item.insert("claudeAiOauth".into(), json!({ "refreshToken": format!("refresh of {GRACE}") }));
        mac.set_item(&item);
        let journal = Journal { from: Some(ADA.into()), to: Some(GRACE.into()), phase: Phase::SetAside };
        save_journal(&mac.ctx, &journal).unwrap();
        // Nothing trusts the half-changed state: the next switch puts Ada back first.
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::AlreadyThere);
        assert_eq!(mac.on().as_deref(), Some(ADA));
        assert!(mac.kept(GRACE).is_some() && mac.kept(ADA).is_none() && mac.journal().is_none());
    }

    /// Interrupted after everything was in place: the copy put back is forgotten first, so it is
    /// never put back a second time.
    #[test]
    fn an_interrupted_switch_is_finished_before_anything_else() {
        let mac = Mac::new();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        mac.login(ADA);
        mac.follow(Some(GRACE)).unwrap();
        // As if the copy of Grace put back had not been forgotten yet.
        mac.live().keep(GRACE, &mac.item().unwrap(), mac.oauth_account()).unwrap();
        let journal = Journal { from: Some(ADA.into()), to: Some(GRACE.into()), phase: Phase::Committed };
        save_journal(&mac.ctx, &journal).unwrap();
        mac.follow(None).unwrap();
        assert!(mac.kept(GRACE).is_none() && mac.journal().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn claude_codes_locks_are_waited_for_kept_fresh_and_given_back() {
        let mac = Mac::new();
        mac.login(ADA);
        let refresh = mac.tmp.path().join(".claude/.oauth_refresh.lock");
        // Held by a running claude: given up on, nothing changed.
        fs::create_dir(&refresh).unwrap();
        assert!(DirLock::acquire_within(&refresh, Duration::from_secs(60), Duration::from_millis(300)).is_err());
        // Left behind long ago: taken over.
        drop(DirLock::acquire_within(&refresh, Duration::ZERO, Duration::from_millis(300)).unwrap());
        assert!(!refresh.exists());
        // Kept fresh while held, so nobody takes it for one left behind.
        let held = DirLock::acquire(&refresh, Duration::from_secs(60)).unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        fs::File::open(&refresh).unwrap().set_modified(old).unwrap();
        std::thread::sleep(DirLock::REFRESH + Duration::from_millis(300));
        let age = SystemTime::now().duration_since(fs::metadata(&refresh).unwrap().modified().unwrap()).unwrap();
        assert!(age < Duration::from_secs(5), "{age:?}");
        drop(held);
        mac.follow(Some(GRACE)).unwrap();
        for lock in [refresh, mac.tmp.path().join(".claude.lock"), mac.tmp.path().join(".claude.json.lock")] {
            assert!(!lock.exists(), "{}", lock.display());
        }
    }

    /// Something at a lock's place that cannot be removed: waited for, then given up on.
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
    fn the_real_keychain_is_never_used_with_a_stand_in_claude() {
        let mac = Mac::new();
        let mut paths = mac.ctx.paths.clone();
        paths.keychain_dir = None;
        let fake = FakeDesktop { running: Some(false), active: None };
        let ctx = Ctx::new(paths, Config::default(), fake, LogSink::Silent);
        assert!(secrets::open(&ctx).is_err());
    }
}
