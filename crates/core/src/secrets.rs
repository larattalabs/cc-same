//! Where sign-ins that must stay secret are kept: the keychain on macOS, through Apple's
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

/// The two places secrets are found: where Claude Code looks for its own item (the keychains it
/// searches), and where CC Same keeps the sign-ins it sets aside (the login keychain).
pub struct Stores {
    pub live: Box<dyn Secrets>,
    pub kept: Box<dyn Secrets>,
}

/// The keychains to use for `ctx`.
pub fn open(ctx: &Ctx) -> Result<Stores> {
    if let Some(dir) = &ctx.paths.keychain_dir {
        return Ok(Stores { live: Box::new(Folder(dir.clone())), kept: Box::new(Folder(dir.clone())) });
    }
    if ctx.fake.running.is_some() {
        bail!("a stand-in Claude Desktop never uses the real keychain (set CC_SAME_KEYCHAIN_DIR)");
    }
    if !cfg!(target_os = "macos") {
        bail!("the keychain is only used on macOS");
    }
    let login = crate::paths::home().join("Library/Keychains/login.keychain-db");
    Ok(Stores { live: Box::new(Keychain { file: None }), kept: Box::new(Keychain { file: Some(login) }) })
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

/// A keychain, through `/usr/bin/security` (by its full path: a `security` earlier on `PATH` must
/// not see the secrets). `file` names one keychain; without it, the user's search list is used,
/// as Claude Code does for its own item.
pub struct Keychain {
    pub file: Option<PathBuf>,
}

const SECURITY: &str = "/usr/bin/security";
/// `errSecItemNotFound`.
const NOT_FOUND: i32 = 44;
/// `security -i` reads its commands with a 4096-byte line buffer, and cuts longer ones short.
const LINE_LIMIT: usize = 4096 - 64;
/// A healthy keychain answers in well under a second; a locked one may wait for a password
/// nobody types.
const TIMEOUT: Duration = Duration::from_secs(5);

struct Output {
    code: Option<i32>,
    stdout: String,
}

fn security(args: &[&str], stdin: Option<&str>) -> Result<Output> {
    let mut child = Command::new(SECURITY)
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    let mut out = Output { code: status.code(), stdout: String::new() };
    if let Some(mut p) = child.stdout.take() {
        p.read_to_string(&mut out.stdout)?;
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

/// What `security find-generic-password -w` printed, as the value stored. It prints a value as it
/// is when every byte is printable, and as hex otherwise (UTF-8 beyond ASCII included). The
/// secrets kept here are JSON objects, so hex that decodes to one is taken for that.
fn printed_value(printed: &str) -> String {
    let printed = printed.strip_suffix('\n').unwrap_or(printed);
    let is_hex = printed.len() % 2 == 0 && !printed.is_empty() && printed.bytes().all(|b| b.is_ascii_hexdigit());
    if is_hex {
        let bytes: Option<Vec<u8>> =
            (0..printed.len()).step_by(2).map(|i| u8::from_str_radix(&printed[i..i + 2], 16).ok()).collect();
        if let Some(text) = bytes.and_then(|b| String::from_utf8(b).ok()) {
            if text.trim_start().starts_with('{') {
                return text;
            }
        }
    }
    printed.to_string()
}

impl Keychain {
    fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = args.to_vec();
        if let Some(file) = self.file.as_ref().and_then(|f| f.to_str()) {
            all.push(file);
        }
        all
    }
}

impl Secrets for Keychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = security(&self.args(&["find-generic-password", "-a", account, "-s", service, "-w"]), None)?;
        match out.code {
            Some(0) => Ok(Some(printed_value(&out.stdout))),
            Some(NOT_FOUND) => Ok(None),
            code => bail!("reading the keychain failed ({code:?})"),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<()> {
        // The value goes in as hex on standard input: never on a command line, where other
        // programs could read it, and never escaped wrong.
        let keychain = self.file.as_ref().map(|f| format!(" {}", quote(&f.to_string_lossy()))).unwrap_or_default();
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {}{keychain}\n",
            quote(account),
            quote(service),
            hex(value.as_bytes())
        );
        if line.len() > LINE_LIMIT {
            bail!("this sign-in is too large to keep in the keychain safely");
        }
        let out = security(&["-i"], Some(&line))?;
        if out.code != Some(0) {
            bail!("writing to the keychain failed ({:?})", out.code);
        }
        // `security -i` can report success for a command it did not carry out: read it back.
        if self.get(service, account)?.as_deref() != Some(value) {
            bail!("the keychain did not keep what was written");
        }
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let delete = |args: Vec<&str>| -> Result<()> {
            let out = security(&args, None)?;
            match out.code {
                Some(0) | Some(NOT_FOUND) => Ok(()),
                code => bail!("removing from the keychain failed ({code:?})"),
            }
        };
        delete(self.args(&["delete-generic-password", "-a", account, "-s", service]))?;
        // Copies of the same name elsewhere in the search list (an older one, made before kept
        // copies were pinned to one keychain) would still answer: remove those too.
        let anywhere = Keychain { file: None };
        for _ in 0..8 {
            if anywhere.get(service, account)?.is_none() {
                return Ok(());
            }
            delete(vec!["delete-generic-password", "-a", account, "-s", service])?;
        }
        if anywhere.get(service, account)?.is_none() {
            return Ok(());
        }
        bail!("the keychain still has another copy of it")
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
    fn hex_printed_for_non_ascii_values_reads_back_as_the_value() {
        let value = r#"{"displayName":"José"}"#;
        assert_eq!(printed_value(&format!("{}\n", hex(value.as_bytes()))), value);
        assert_eq!(printed_value("{\"a\":1}\n"), "{\"a\":1}");
        // Hex that is not a JSON object is a value of its own.
        assert_eq!(printed_value("deadbeef\n"), "deadbeef");
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
