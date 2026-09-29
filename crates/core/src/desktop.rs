//! Is Claude Desktop running, and which account has it loaded?
//!
//! Desktop writes only the loaded account's index and reads it only when that account
//! loads, so this decides what we may touch. Signals, most exact first:
//! * `main.log`: `[LocalSessionManager] Initialization succeeded — accountId=…, orgId=…`
//! * `config.json`: `lastKnownAccountUuid`
//!
//! When Desktop runs but neither signal is readable, every index counts as loaded.

use crate::ctx::{Ctx, LogScan};
use crate::fsx;
use crate::model::{is_uuid, AppState};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const INIT_MARKER: &str = "[LocalSessionManager] Initialization succeeded";
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

pub fn detect(ctx: &Ctx) -> AppState {
    let running = is_running(ctx);
    let mut accounts = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    let mut sources = Vec::new();
    if let Some(active) = &ctx.fake.active {
        for item in active {
            match item.split_once('/') {
                Some((a, o)) => {
                    pairs.insert((a.to_string(), o.to_string()));
                    accounts.insert(a.to_string());
                }
                None => {
                    accounts.insert(item.clone());
                }
            }
        }
        sources.push("override");
    }
    let mut init_marker = None;
    if ctx.fake.active.is_none() {
        if let Some(acct) = last_known_account(ctx) {
            accounts.insert(acct);
            sources.push("config.json");
        }
    }
    if let Some(init) = last_init(ctx) {
        init_marker = Some(init.marker);
        if ctx.fake.active.is_none() {
            accounts.insert(init.acct.clone());
            pairs.insert((init.acct, init.org));
            sources.push("Desktop log");
        }
    }
    // Keep a just-left account read-only for a moment: Desktop flushes it after switching.
    let grace = Duration::from_secs_f64(ctx.config().switch_grace_seconds.max(0.0));
    let mut recent = ctx.caches.recent_accounts.lock().unwrap();
    if running {
        for a in &accounts {
            recent.insert(a.clone(), Instant::now());
        }
    }
    recent.retain(|_, t| t.elapsed() <= grace);
    accounts.extend(recent.keys().cloned());
    AppState {
        running,
        uncertain: accounts.is_empty(),
        accounts,
        pairs,
        source: if sources.is_empty() { "unknown".into() } else { sources.join(" + ") },
        init_marker,
    }
}

pub fn is_running(ctx: &Ctx) -> bool {
    if let Some(r) = ctx.fake.running {
        return r;
    }
    // If the process table cannot be read, assume Desktop runs: the safe side.
    process::desktop_running().unwrap_or(true)
}

pub fn last_known_account(ctx: &Ctx) -> Option<String> {
    let Ok(Value::Object(cfg)) = fsx::read_json(&ctx.paths.desktop_config(), 16 * 1024 * 1024) else {
        return None;
    };
    cfg.get("lastKnownAccountUuid").and_then(Value::as_str).filter(|s| is_uuid(s)).map(str::to_string)
}

/// The newest "Initialization succeeded" line in Desktop's log.
pub struct InitLine {
    pub acct: String,
    pub org: String,
    /// `<log file name>@<byte offset>`: changes whenever Desktop loads a session list again.
    pub marker: String,
}

pub fn last_init(ctx: &Ctx) -> Option<InitLine> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&ctx.paths.desktop_logs)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("main") && n.ends_with(".log")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut scans = ctx.caches.log_scan.lock().unwrap();
    for (_, path) in files.into_iter().take(4) {
        let scan = scans.entry(path.clone()).or_default();
        if let Some((acct, org)) = scan_log(&path, scan) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Some(InitLine { acct, org, marker: format!("{name}@{}", scan.found_at) });
        }
    }
    None
}

/// Account and org from the newest "Initialization succeeded" line in Desktop's log.
pub fn last_init_pair(ctx: &Ctx) -> Option<(String, String)> {
    last_init(ctx).map(|i| (i.acct, i.org))
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == needle[0] && &hay[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Re-read only what was appended since the last call (with some overlap).
fn scan_log(path: &Path, scan: &mut LogScan) -> Option<(String, String)> {
    let len = fs::metadata(path).ok()?.len();
    if len < scan.offset {
        *scan = LogScan::default(); // rotated or truncated
    }
    if scan.offset == 0 {
        scan.offset = len.saturating_sub(TAIL_BYTES);
    }
    let start = scan.offset.saturating_sub(512);
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    let needle = INIT_MARKER.as_bytes();
    for idx in find_all(&buf, needle) {
        let from = idx + needle.len();
        let tail = String::from_utf8_lossy(&buf[from..buf.len().min(from + 200)]);
        if let (Some(acct), Some(org)) = (value_after(&tail, "accountId="), value_after(&tail, "orgId=")) {
            scan.found = Some((acct, org));
            scan.found_at = start + idx as u64;
        }
    }
    scan.offset = start + buf.len() as u64;
    scan.found.clone()
}

fn value_after(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)? + key.len();
    let v = text.get(at..at + 36)?;
    is_uuid(v).then(|| v.to_string())
}

/// Desktop's version, where it can be read cheaply.
pub fn desktop_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let plist = fs::read_to_string("/Applications/Claude.app/Contents/Info.plist").ok()?;
        let at = plist.find("<key>CFBundleShortVersionString</key>")?;
        let rest = &plist[at..];
        let start = rest.find("<string>")? + "<string>".len();
        let end = rest[start..].find("</string>")?;
        Some(rest[start..start + end].trim().to_string())
    }
    #[cfg(windows)]
    {
        // Squirrel installs live in %LOCALAPPDATA%\AnthropicClaude\app-<version>.
        let dir = dirs::data_local_dir()?.join("AnthropicClaude");
        let mut versions: Vec<String> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("app-").map(str::to_string))
            .collect();
        versions.sort_by_key(|v| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>());
        versions.pop()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

mod process {
    //! A tiny process-table reader per platform, just enough to spot Claude Desktop.

    #[cfg(target_os = "macos")]
    pub fn desktop_running() -> Option<bool> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return None;
        }
        let mut pids = vec![0i32; count as usize + 256];
        let size = (pids.len() * std::mem::size_of::<i32>()) as i32;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        if count <= 0 {
            return None;
        }
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        for &pid in pids.iter().take(count as usize) {
            if pid <= 0 {
                continue;
            }
            let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
            if n > 0 && path[..n as usize].ends_with(b".app/Contents/MacOS/Claude") {
                return Some(true);
            }
        }
        Some(false)
    }

    #[cfg(target_os = "linux")]
    pub fn desktop_running() -> Option<bool> {
        // Linux builds are unofficial (e.g. claude-desktop-debian): an Electron process whose
        // executable or command line names claude-desktop. The Claude Code CLI ("claude") is
        // deliberately not matched.
        let entries = std::fs::read_dir("/proc").ok()?;
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit())) else { continue };
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            let comm = comm.trim();
            if comm == "claude-desktop" || comm == "Claude" {
                return Some(true);
            }
            if comm == "electron" || comm.starts_with("claude") {
                let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                if String::from_utf8_lossy(&cmd).contains("claude-desktop") {
                    return Some(true);
                }
            }
        }
        Some(false)
    }

    #[cfg(windows)]
    pub fn desktop_running() -> Option<bool> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = false;
            let mut ok = Process32FirstW(snap, &mut entry) != 0;
            while ok {
                let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if exe.eq_ignore_ascii_case("claude.exe") {
                    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                    if !handle.is_null() {
                        let mut buf = [0u16; 1024];
                        let mut size = buf.len() as u32;
                        if QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) != 0 {
                            let path = String::from_utf16_lossy(&buf[..size as usize]).to_lowercase();
                            // Desktop lives in ...\AnthropicClaude\app-<version>\ or a Store
                            // package; the Claude Code CLI (also claude.exe) does not.
                            if path.contains("anthropicclaude")
                                || path.contains("\\windowsapps\\claude")
                                || path.contains("\\app-")
                            {
                                found = true;
                            }
                        }
                        CloseHandle(handle);
                    }
                }
                if found {
                    break;
                }
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
            Some(found)
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub fn desktop_running() -> Option<bool> {
        None
    }
}
