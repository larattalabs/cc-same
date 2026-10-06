//! Taking Claude Code's command line along when Claude Desktop switches accounts (`switchCli`,
//! off unless turned on; macOS).
//!
//! The command line keeps a sign-in of its own, apart from Desktop's: its tokens in the login
//! keychain (`Claude Code-credentials`, under the user's name) and who they belong to in
//! `oauthAccount` of `~/.claude.json`. A switch sets that sign-in aside in the keychain, under
//! `cc-same CLI sign-in` and the account's ID, and puts back the one kept for the account Claude
//! switches to. Nothing ever leaves the keychain for a file, and nothing is sent anywhere.
//!
//! * The sign-in set aside is the one in use, whoever it belongs to: the command line can be
//!   signed in to another account than Desktop was.
//! * When nothing is kept for the account switched to yet, the command line is signed out, its
//!   sign-in kept like any other, and `claude` asks to sign in: `/login` there, once, to the
//!   account Claude switched to. From then on that account switches back and forth like the rest.
//!   (Signing in over the old sign-in instead would end it with nothing kept.)
//! * Tokens rotate as they are used, so a copy put back is not kept: it is set aside afresh when
//!   it is left.
//! * Claude Code refreshes its tokens under two lock folders, and writes `~/.claude.json` under a
//!   third. A switch holds all three, so a `claude` running meanwhile finishes a refresh first, or
//!   finds the new account's tokens and keeps them. Only `oauthAccount` in `~/.claude.json`
//!   changes; everything else in it stays as it was.
//! * Only the default configuration folder: with `CLAUDE_CONFIG_DIR` set, Claude Code names its
//!   keychain item after that folder, and the command line is left alone.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::is_uuid;
use crate::secrets::{self, Secrets};
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Claude Code's own keychain item.
const LIVE_SERVICE: &str = "Claude Code-credentials";
/// Ours, one per account.
const KEPT_SERVICE: &str = "cc-same CLI sign-in";

/// What became of the command line's sign-in in a switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum Followed {
    /// Signed in to the account Claude switched to now.
    Switched { from: String },
    /// Already signed in to it.
    AlreadyThere,
    /// Nothing kept for that account yet: signed out, the sign-in of `from` kept.
    SignedOut { from: String },
    /// Not signed in, or signed in some way CC Same does not switch.
    NotSignedIn,
    /// Claude signed out to add an account: the command line stays signed in to `on`.
    Stayed { on: String },
}

/// A sign-in set aside: the keychain value as Claude Code wrote it, and who it belongs to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Kept {
    credentials: String,
    oauth_account: Map<String, Value>,
    set_aside_at: f64,
}

/// Bring the command line to `to` (or leave it be, for `None`). Claude Desktop has quit.
pub fn follow(ctx: &Ctx, to: Option<&str>) -> Result<Followed> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        bail!("CLAUDE_CONFIG_DIR is set, so Claude Code keeps its sign-in elsewhere; the command line was left alone");
    }
    let keychain = secrets::open(ctx)?;
    follow_with(ctx, keychain.as_ref(), to)
}

fn follow_with(ctx: &Ctx, keychain: &dyn Secrets, to: Option<&str>) -> Result<Followed> {
    let home = claude_home(ctx);
    let _refresh = DirLock::acquire(&home.join(".oauth_refresh.lock"), Duration::from_secs(60))?;
    let _legacy = DirLock::acquire(&lock_path(&home), Duration::from_secs(60))?;
    let _config = DirLock::acquire(&lock_path(&ctx.paths.claude_json), Duration::from_secs(10))?;
    let user = user_name();

    let mut config = read_config(&ctx.paths.claude_json)?;
    let live = keychain.get(LIVE_SERVICE, &user)?;
    let on = config
        .get("oauthAccount")
        .and_then(|o| o.get("accountUuid"))
        .and_then(Value::as_str)
        .filter(|a| is_uuid(a))
        .map(str::to_string);
    let (Some(on), Some(live)) = (on, live) else { return Ok(Followed::NotSignedIn) };
    let Some(to) = to else { return Ok(Followed::Stayed { on }) };
    if on == to {
        // A copy kept earlier is out of date now.
        keychain.delete(KEPT_SERVICE, to)?;
        return Ok(Followed::AlreadyThere);
    }
    let target: Option<Kept> = match keychain.get(KEPT_SERVICE, to)? {
        None => None,
        Some(text) => {
            let kept: Kept = serde_json::from_str(&text).context("reading the sign-in kept for that account")?;
            if kept.credentials.is_empty() || kept.oauth_account.get("accountUuid").and_then(Value::as_str) != Some(to)
            {
                bail!("the command-line sign-in kept for that account is damaged; sign in to it again with /login in claude");
            }
            Some(kept)
        }
    };

    // Set the one in use aside, then put the target's in its place (or nobody's); undo that if
    // it fails.
    let outgoing = Kept {
        credentials: live.clone(),
        oauth_account: config.get("oauthAccount").and_then(Value::as_object).cloned().unwrap_or_default(),
        set_aside_at: fsx::now_secs(),
    };
    keychain.set(KEPT_SERVICE, &on, &serde_json::to_string(&outgoing)?)?;
    let mut put_back = || -> Result<()> {
        match &target {
            Some(kept) => {
                keychain.set(LIVE_SERVICE, &user, &kept.credentials)?;
                config.insert("oauthAccount".into(), Value::Object(kept.oauth_account.clone()));
            }
            None => {
                keychain.delete(LIVE_SERVICE, &user)?;
                config.remove("oauthAccount");
            }
        }
        write_config(&ctx.paths.claude_json, &config)
    };
    if let Err(e) = put_back() {
        keychain.set(LIVE_SERVICE, &user, &live).context("putting the command line's sign-in back")?;
        // In use again, so not kept: a copy is never put back twice.
        let _ = keychain.delete(KEPT_SERVICE, &on);
        return Err(e);
    }
    if target.is_none() {
        return Ok(Followed::SignedOut { from: on });
    }
    // Put back, so no longer kept: it is set aside afresh when it is left.
    if let Err(e) = keychain.delete(KEPT_SERVICE, to) {
        ctx.log(format!("forgetting the copy put back: {e:#}"));
    }
    Ok(Followed::Switched { from: on })
}

/// Accounts with a command-line sign-in kept, of those given.
pub fn kept(ctx: &Ctx, accounts: &[String]) -> Vec<String> {
    let Ok(keychain) = secrets::open(ctx) else { return Vec::new() };
    accounts.iter().filter(|a| keychain.get(KEPT_SERVICE, a).ok().flatten().is_some()).cloned().collect()
}

/// Forget the command-line sign-in kept for `account`.
pub fn forget(ctx: &Ctx, account: &str) -> Result<()> {
    secrets::open(ctx)?.delete(KEPT_SERVICE, account)
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

/// One of Claude Code's locks: a folder, made to take it and removed to give it back. One older
/// than `stale` was left by a run that ended without giving it back. Holding it for longer than
/// a moment would need refreshing its time, as Claude Code does; a switch holds it for less.
struct DirLock(PathBuf);

impl DirLock {
    const WAIT: Duration = Duration::from_secs(10);

    fn acquire(path: &Path, stale: Duration) -> Result<DirLock> {
        Self::acquire_within(path, stale, Self::WAIT)
    }

    fn acquire_within(path: &Path, stale: Duration, wait: Duration) -> Result<DirLock> {
        let deadline = Instant::now() + wait;
        loop {
            match fs::create_dir(path) {
                Ok(()) => return Ok(DirLock(path.to_path_buf())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > stale) {
                        let _ = fs::remove_dir(path);
                        continue;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Its folder is missing: nothing of Claude Code's to guard there.
                    return Ok(DirLock(PathBuf::new()));
                }
                Err(e) => return Err(e).with_context(|| format!("taking {}", path.display())),
            }
            if Instant::now() >= deadline {
                bail!("Claude Code is busy with its sign-in ({} is taken); try again in a moment", path.display());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_dir(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Folder;
    use crate::{Config, FakeDesktop, LogSink, Paths};

    const ADA: &str = "5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d";
    const GRACE: &str = "9e31ea7e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

    struct Mac {
        tmp: tempfile::TempDir,
        ctx: Ctx,
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
            let keychain = Folder(tmp.path().join("keychain"));
            let fake = FakeDesktop { running: Some(false), active: None };
            let ctx = Ctx::new(paths, Config::default(), fake, LogSink::Silent);
            Mac { tmp, ctx, keychain }
        }

        /// What `/login` in claude does: its tokens in the keychain, who they belong to in the config.
        fn login(&self, account: &str) {
            self.keychain.set(LIVE_SERVICE, &user_name(), &format!("tokens of {account}")).unwrap();
            let mut config = read_config(&self.ctx.paths.claude_json).unwrap();
            if config.is_empty() {
                config.insert("numStartups".into(), 7.into());
                config.insert("projects".into(), serde_json::json!({"/tmp/p": {"allowedTools": []}}));
            }
            let email = format!("{}@example.com", &account[..4]);
            config.insert("oauthAccount".into(), serde_json::json!({ "accountUuid": account, "emailAddress": email }));
            write_config(&self.ctx.paths.claude_json, &config).unwrap();
        }

        /// Who the command line is signed in to, and with what.
        fn on(&self) -> Option<(String, String)> {
            let config = read_config(&self.ctx.paths.claude_json).unwrap();
            let who = config.get("oauthAccount").map(|o| o["accountUuid"].as_str().unwrap().to_string());
            let tokens = self.keychain.get(LIVE_SERVICE, &user_name()).unwrap();
            assert_eq!(who.is_some(), tokens.is_some(), "signed in halfway");
            who.zip(tokens)
        }

        fn kept(&self, account: &str) -> Option<Kept> {
            self.keychain.get(KEPT_SERVICE, account).unwrap().map(|k| serde_json::from_str(&k).unwrap())
        }

        fn follow(&self, to: Option<&str>) -> Result<Followed> {
            follow_with(&self.ctx, &self.keychain, to)
        }
    }

    fn tokens(account: &str) -> Option<(String, String)> {
        Some((account.into(), format!("tokens of {account}")))
    }

    #[test]
    fn the_command_line_follows_from_its_second_switch_on() {
        let mac = Mac::new();
        mac.login(ADA);
        // Nothing kept for Grace yet: signed out, Ada's sign-in kept, until /login to Grace.
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::SignedOut { from: ADA.into() });
        assert_eq!(mac.on(), None);
        assert_eq!(mac.kept(ADA).unwrap().credentials, format!("tokens of {ADA}"));
        mac.login(GRACE);
        // From now on, both switch.
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::Switched { from: GRACE.into() });
        assert_eq!(mac.on(), tokens(ADA));
        // Ada's copy was put back, so it is no longer kept; Grace's is.
        assert!(mac.kept(ADA).is_none() && mac.kept(GRACE).is_some());
        // Tokens rotate while claude runs: switching away keeps the newest.
        mac.keychain.set(LIVE_SERVICE, &user_name(), "rotated tokens of ada").unwrap();
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::Switched { from: ADA.into() });
        assert_eq!(mac.on(), tokens(GRACE));
        let ada = mac.kept(ADA).unwrap();
        assert_eq!(ada.credentials, "rotated tokens of ada");
        assert_eq!(ada.oauth_account["emailAddress"], "5a1d@example.com");
        assert_eq!(mac.follow(Some(GRACE)).unwrap(), Followed::AlreadyThere);
    }

    #[test]
    fn everything_else_in_the_config_stays() {
        let mac = Mac::new();
        mac.login(ADA);
        let before = read_config(&mac.ctx.paths.claude_json).unwrap();
        mac.follow(Some(GRACE)).unwrap();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        let text = fs::read_to_string(&mac.ctx.paths.claude_json).unwrap();
        assert!(text.starts_with("{\n  \"numStartups\": 7"), "{text}");
        assert_eq!(read_config(&mac.ctx.paths.claude_json).unwrap(), before);
    }

    #[test]
    fn signing_out_to_add_an_account_leaves_the_command_line_be() {
        let mac = Mac::new();
        mac.login(ADA);
        assert_eq!(mac.follow(None).unwrap(), Followed::Stayed { on: ADA.into() });
        assert_eq!(mac.on(), tokens(ADA));
        assert!(mac.kept(ADA).is_none());
    }

    #[test]
    fn not_signed_in_means_nothing_to_do() {
        let mac = Mac::new();
        assert_eq!(mac.follow(Some(ADA)).unwrap(), Followed::NotSignedIn);
    }

    #[test]
    fn a_damaged_copy_changes_nothing() {
        let mac = Mac::new();
        mac.login(ADA);
        let damaged = r#"{"credentials":"x","oauthAccount":{"accountUuid":"someone else"}}"#;
        mac.keychain.set(KEPT_SERVICE, GRACE, damaged).unwrap();
        assert!(mac.follow(Some(GRACE)).is_err());
        assert_eq!(mac.on(), tokens(ADA));
        assert!(mac.kept(ADA).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_switch_puts_the_sign_in_back() {
        let mac = Mac::new();
        mac.login(GRACE);
        mac.follow(Some(ADA)).unwrap();
        mac.login(ADA);
        // ~/.claude.json cannot be written (it is a symlink, which is never written through), so
        // the switch fails after Grace's tokens went in: the keychain must not be left on Grace.
        let config = &mac.ctx.paths.claude_json;
        let real = mac.tmp.path().join("claude.json.real");
        fs::rename(config, &real).unwrap();
        std::os::unix::fs::symlink(&real, config).unwrap();
        let result = mac.follow(Some(GRACE));
        assert!(result.is_err());
        assert_eq!(mac.on(), tokens(ADA));
        // Grace's copy is still kept, to try again; Ada's is in use, so it is not.
        assert!(mac.kept(GRACE).is_some() && mac.kept(ADA).is_none());
    }

    #[test]
    fn claude_codes_locks_are_waited_for_and_given_back() {
        let mac = Mac::new();
        mac.login(ADA);
        let refresh = mac.tmp.path().join(".claude/.oauth_refresh.lock");
        // Held by a running claude: given up on, nothing changed.
        fs::create_dir(&refresh).unwrap();
        assert!(DirLock::acquire_within(&refresh, Duration::from_secs(60), Duration::from_millis(300)).is_err());
        // Left behind long ago: taken over.
        drop(DirLock::acquire_within(&refresh, Duration::ZERO, Duration::from_millis(300)).unwrap());
        assert!(!refresh.exists());
        mac.follow(Some(GRACE)).unwrap();
        for lock in [refresh, mac.tmp.path().join(".claude.lock"), mac.tmp.path().join(".claude.json.lock")] {
            assert!(!lock.exists(), "{}", lock.display());
        }
    }

    #[test]
    fn the_real_keychain_is_never_used_with_a_stand_in_claude() {
        let mut mac = Mac::new();
        let mut paths = mac.ctx.paths.clone();
        paths.keychain_dir = None;
        mac.ctx =
            Ctx::new(paths, Config::default(), FakeDesktop { running: Some(false), active: None }, LogSink::Silent);
        assert!(secrets::open(&mac.ctx).is_err());
    }
}
