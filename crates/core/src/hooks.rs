//! Commands to run when something happens, set in `config.json` (`onSwitch`, `onSyncError`): to
//! send a notification somewhere else, say, or to set something up for the account Claude
//! switched to. A hook runs through the shell, with what happened in its environment:
//!
//! | | |
//! | --- | --- |
//! | `CC_SAME_EVENT` | `switch` or `sync-error` |
//! | `CC_SAME_MESSAGE` | one sentence saying what happened |
//! | `CC_SAME_FROM`, `CC_SAME_TO` | the accounts switched from and to (empty when signed out) |
//! | `CC_SAME_FROM_EMAIL`, `CC_SAME_TO_EMAIL` | their emails, when known |
//! | `CC_SAME_ERRORS` | how many changes failed (`sync-error`) |
//!
//! A hook never holds anything up: it starts and CC Same carries on, and one still running after
//! a minute is stopped. A sync error runs its hook once, when syncing starts failing, not on every
//! pass while it keeps failing.

use crate::ctx::Ctx;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Switch { from: Option<String>, to: Option<String> },
    SyncError { errors: usize, first: String },
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::Switch { .. } => "switch",
            Event::SyncError { .. } => "sync-error",
        }
    }
}

/// Run the hook set for `event`, if there is one.
pub fn run(ctx: &Ctx, event: &Event) {
    let cfg = ctx.config();
    let command = match event {
        Event::Switch { .. } => cfg.on_switch,
        Event::SyncError { .. } => cfg.on_sync_error,
    };
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else { return };
    let labels = crate::scan::account_labels(ctx);
    let label = |a: &Option<String>| match a {
        Some(a) => labels.get(a).cloned().unwrap_or_else(|| format!("account {}", crate::short(a))),
        None => "nobody".to_string(),
    };
    let mut env: Vec<(&str, String)> = vec![("CC_SAME_EVENT", event.name().into())];
    match event {
        Event::Switch { from, to } => {
            env.push(("CC_SAME_MESSAGE", format!("Claude switched from {} to {}", label(from), label(to))));
            env.push(("CC_SAME_FROM", from.clone().unwrap_or_default()));
            env.push(("CC_SAME_TO", to.clone().unwrap_or_default()));
            let email = |a: &Option<String>| a.as_ref().and_then(|a| labels.get(a)).cloned().unwrap_or_default();
            env.push(("CC_SAME_FROM_EMAIL", email(from)));
            env.push(("CC_SAME_TO_EMAIL", email(to)));
        }
        Event::SyncError { errors, first } => {
            env.push(("CC_SAME_MESSAGE", format!("CC Same could not sync {errors} change(s): {first}")));
            env.push(("CC_SAME_ERRORS", errors.to_string()));
        }
    }
    let mut cmd = shell(&command);
    cmd.envs(env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            ctx.log(format!("hook {}: started", event.name()));
            // Reap it, and stop it if it hangs; the caller carries on meanwhile.
            std::thread::spawn(move || {
                let deadline = Instant::now() + TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => return,
                        Ok(None) if Instant::now() >= deadline => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                    }
                }
            });
        }
        Err(e) => ctx.log(format!("hook {}: could not start: {e}", event.name())),
    }
}

#[cfg(unix)]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").raw_arg(command).creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Whether syncing has been failing, so a run of failed passes runs the hook once.
#[derive(Debug, Default)]
pub struct Failing(bool);

impl Failing {
    /// Note a pass's errors; runs the hook when syncing starts failing.
    pub fn pass(&mut self, ctx: &Ctx, errors: &[String]) {
        let failing = !errors.is_empty();
        if failing && !self.0 {
            let first = errors.first().cloned().unwrap_or_default();
            run(ctx, &Event::SyncError { errors: errors.len(), first });
        }
        self.0 = failing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use std::fs;
    use std::path::Path;

    fn ctx(dir: &Path, config: Config) -> Ctx {
        let mut paths = Paths::new(dir.join("Claude"), dir.join("state"));
        paths.claude_json = dir.join(".claude.json");
        Ctx::new(paths, config, FakeDesktop { running: Some(false), active: None }, LogSink::Silent)
    }

    #[cfg(unix)]
    fn wait_for(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(path) {
                if text.ends_with('\n') {
                    return text;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the hook did not run");
    }

    #[cfg(unix)]
    #[test]
    fn a_switch_runs_its_hook_with_what_happened() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let hook = format!("echo \"$CC_SAME_EVENT|$CC_SAME_FROM|$CC_SAME_TO|$CC_SAME_MESSAGE\" > '{}'", out.display());
        let ctx = ctx(tmp.path(), Config { on_switch: Some(hook), ..Config::default() });
        let from = "aaaaaaaa-0000-4000-8000-000000000001".to_string();
        run(&ctx, &Event::Switch { from: Some(from.clone()), to: None });
        assert_eq!(wait_for(&out), format!("switch|{from}||Claude switched from account aaaaaaaa to nobody\n"));
    }

    #[cfg(unix)]
    #[test]
    fn sync_errors_run_their_hook_once_until_syncing_works_again() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let hook = format!("echo \"$CC_SAME_ERRORS $CC_SAME_MESSAGE\" >> '{}'", out.display());
        let ctx = ctx(tmp.path(), Config { on_sync_error: Some(hook), ..Config::default() });
        let mut failing = Failing::default();
        failing.pass(&ctx, &["disk full".into(), "disk full".into()]);
        assert_eq!(wait_for(&out), "2 CC Same could not sync 2 change(s): disk full\n");
        failing.pass(&ctx, &["disk full".into()]);
        failing.pass(&ctx, &[]);
        failing.pass(&ctx, &["locked".into()]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::read_to_string(&out).unwrap().lines().count() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let lines: Vec<String> = fs::read_to_string(&out).unwrap().lines().map(str::to_string).collect();
        assert_eq!(
            lines,
            ["2 CC Same could not sync 2 change(s): disk full", "1 CC Same could not sync 1 change(s): locked"]
        );
    }

    #[test]
    fn no_hook_no_command() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path(), Config::default());
        run(&ctx, &Event::Switch { from: None, to: None });
        Failing::default().pass(&ctx, &["x".into()]);
    }
}
