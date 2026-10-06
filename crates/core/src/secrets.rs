//! Where sign-ins that must stay secret are kept: the login keychain on macOS, through Apple's
//! `security` tool, the way Claude Code keeps its own. A folder can stand in for the keychain in
//! tests and scripts (`CC_SAME_KEYCHAIN_DIR`); without one, a stand-in Claude Desktop never
//! reaches the real keychain.

use crate::ctx::Ctx;
use anyhow::{bail, Context as _, Result};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub trait Secrets {
    /// The secret kept for `account` under `service`, or `None` when there is none.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    /// Create it, or replace what is there.
    fn set(&self, service: &str, account: &str, value: &str) -> Result<()>;
    /// Remove it; one that is not there counts as removed.
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

/// The keychain to use for `ctx`.
pub fn open(ctx: &Ctx) -> Result<Box<dyn Secrets>> {
    if let Some(dir) = &ctx.paths.keychain_dir {
        return Ok(Box::new(Folder(dir.clone())));
    }
    if ctx.fake.running.is_some() {
        bail!("a stand-in Claude Desktop never uses the real keychain (set CC_SAME_KEYCHAIN_DIR)");
    }
    if !cfg!(target_os = "macos") {
        bail!("the keychain is only used on macOS");
    }
    Ok(Box::new(Keychain))
}

/// Files in a folder, one per secret: a stand-in for tests and scripts.
pub struct Folder(pub PathBuf);

impl Folder {
    fn path(&self, service: &str, account: &str) -> PathBuf {
        let safe = |s: &str| {
            s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>()
        };
        self.0.join(format!("{}.{}", safe(service), safe(account)))
    }
}

impl Secrets for Folder {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        match fs::read_to_string(self.path(service, account)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        crate::fsx::create_private_dir_all(&self.0)?;
        crate::fsx::atomic_write(&self.path(service, account), value.as_bytes(), None, false)?;
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        match fs::remove_file(self.path(service, account)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// The login keychain, through `/usr/bin/security` (by its full path: a `security` earlier on
/// `PATH` must not see the secrets). Claude Code makes its item with the same tool, so reading it
/// back asks nothing.
pub struct Keychain;

const SECURITY: &str = "/usr/bin/security";
/// `errSecItemNotFound`.
const NOT_FOUND: i32 = 44;
/// `security -i` reads its commands with a 4096-byte line buffer, and cuts longer ones short.
const LINE_LIMIT: usize = 4096 - 64;
/// A healthy keychain answers in well under a second; a locked one may wait for a password
/// nobody types.
const TIMEOUT: Duration = Duration::from_secs(10);

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn security(args: &[&str], stdin: Option<&str>) -> Result<Output> {
    let mut child = Command::new(SECURITY)
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting security")?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).context("talking to security")?;
    }
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the keychain did not answer (is it locked?)");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = Output { code: status.code(), stdout: String::new(), stderr: String::new() };
    if let Some(mut p) = child.stdout.take() {
        p.read_to_string(&mut out.stdout)?;
    }
    if let Some(mut p) = child.stderr.take() {
        p.read_to_string(&mut out.stderr)?;
    }
    Ok(out)
}

/// A value for a `security -i` command line, quoted the way it reads them back.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Secrets for Keychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = security(&["find-generic-password", "-a", account, "-s", service, "-w"], None)?;
        match out.code {
            Some(0) => Ok(Some(out.stdout.strip_suffix('\n').unwrap_or(&out.stdout).to_string())),
            Some(NOT_FOUND) => Ok(None),
            code => bail!("reading the keychain failed ({code:?}): {}", out.stderr.trim()),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        // The value goes in as hex on standard input: never on a command line, where other
        // programs could read it, and never escaped wrong.
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {}\n",
            quote(account),
            quote(service),
            hex(value.as_bytes())
        );
        if line.len() > LINE_LIMIT {
            bail!("this sign-in is too large to keep in the keychain safely");
        }
        let out = security(&["-i"], Some(&line))?;
        if out.code != Some(0) {
            bail!("writing to the keychain failed ({:?}): {}", out.code, out.stderr.trim());
        }
        // `security -i` can report success for a command it did not carry out: read it back.
        if self.get(service, account)?.as_deref() != Some(value) {
            bail!("the keychain did not keep what was written");
        }
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let out = security(&["delete-generic-password", "-a", account, "-s", service], None)?;
        match out.code {
            Some(0) | Some(NOT_FOUND) => Ok(()),
            code => bail!("removing from the keychain failed ({code:?}): {}", out.stderr.trim()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_hex_match_what_security_reads() {
        assert_eq!(quote(r#"Claude Code-credentials"#), r#""Claude Code-credentials""#);
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(hex(b"{\"a\":1}"), "7b2261223a317d");
    }

    #[test]
    fn a_folder_stands_in_for_the_keychain() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Folder(tmp.path().join("kc"));
        assert_eq!(f.get("svc", "me").unwrap(), None);
        f.set("svc", "me", "secret").unwrap();
        f.set("svc", "me", "newer").unwrap();
        assert_eq!(f.get("svc", "me").unwrap().as_deref(), Some("newer"));
        f.delete("svc", "me").unwrap();
        f.delete("svc", "me").unwrap();
        assert_eq!(f.get("svc", "me").unwrap(), None);
    }
}
