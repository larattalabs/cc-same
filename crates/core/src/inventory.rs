//! What stays with each account: the connectors its sessions use, the plugins its organization
//! hands out, and the artifacts it published. None of it travels with a session (see
//! [`crate::model::ACCOUNT_LOCAL_KEYS`]), so after a switch it is set up again, or shared, by
//! hand. This says what that is.
//!
//! Everything is read from disk, as Claude left it; nothing is asked of Anthropic:
//! * connectors from `remoteMcpServersConfig` in the account's own session records. A session
//!   names only the connectors it ran with, so these are the ones seen, not every one connected;
//! * plugins from Claude Code's synced copy of the organization's,
//!   `~/.claude/plugins/synced/<org>_<account>/manifest.json`;
//! * artifacts from `publishedArtifacts` in the account's own session records.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::{is_uuid, Surface, MAX_RECORD_BYTES};
use crate::scan;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Connector {
    pub name: String,
    pub url: Option<String>,
}

impl Connector {
    /// The same connector, by name: its address changes with the server's version.
    fn key(&self) -> String {
        self.name.to_lowercase()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Plugin {
    pub name: String,
    pub marketplace: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub url: String,
    pub title: Option<String>,
    /// Unix seconds.
    pub updated_at: Option<f64>,
}

/// What one account has that others may not.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Holdings {
    pub account: String,
    pub connectors: Vec<Connector>,
    pub plugins: Vec<Plugin>,
    /// Newest first.
    pub artifacts: Vec<Artifact>,
}

/// What an account lacks that another account has.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Missing {
    pub connectors: Vec<Connector>,
    pub plugins: Vec<Plugin>,
}

impl Missing {
    pub fn is_empty(&self) -> bool {
        self.connectors.is_empty() && self.plugins.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Inventory {
    pub accounts: Vec<Holdings>,
}

impl Inventory {
    pub fn of(&self, account: &str) -> Option<&Holdings> {
        self.accounts.iter().find(|h| h.account == account)
    }

    /// The connectors and plugins some other account has and `account` does not.
    pub fn missing(&self, account: &str) -> Missing {
        let empty = Holdings::default();
        let mine = self.of(account).unwrap_or(&empty);
        let have: BTreeSet<String> = mine.connectors.iter().map(Connector::key).collect();
        let have_plugins: BTreeSet<&str> = mine.plugins.iter().map(|p| p.name.as_str()).collect();
        let mut connectors: BTreeMap<String, Connector> = BTreeMap::new();
        let mut plugins: BTreeMap<String, Plugin> = BTreeMap::new();
        for other in self.accounts.iter().filter(|h| h.account != account) {
            for c in other.connectors.iter().filter(|c| !have.contains(&c.key())) {
                connectors.entry(c.key()).or_insert_with(|| c.clone());
            }
            for p in other.plugins.iter().filter(|p| !have_plugins.contains(p.name.as_str())) {
                plugins.entry(p.name.clone()).or_insert_with(|| p.clone());
            }
        }
        Missing { connectors: connectors.into_values().collect(), plugins: plugins.into_values().collect() }
    }
}

/// Read every account's holdings. Accounts come from the session folders and the synced plugins.
pub fn read(ctx: &Ctx) -> Inventory {
    let mut connectors: BTreeMap<String, BTreeMap<String, Connector>> = BTreeMap::new();
    let mut artifacts: BTreeMap<String, BTreeMap<String, Artifact>> = BTreeMap::new();
    for part in scan::discover(ctx, Surface::Code) {
        connectors.entry(part.acct.clone()).or_default();
        let Ok(files) = fs::read_dir(&part.path) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("local_") && name.ends_with(".json")) {
                continue;
            }
            let Ok(Value::Object(d)) = fsx::read_json(&f.path(), MAX_RECORD_BYTES) else { continue };
            for c in d.get("remoteMcpServersConfig").and_then(Value::as_array).into_iter().flatten() {
                let name = c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty());
                let url = c.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string);
                if let Some(name) = name {
                    let c = Connector { name: name.to_string(), url };
                    connectors.entry(part.acct.clone()).or_default().entry(c.key()).or_insert(c);
                }
            }
            for a in d.get("publishedArtifacts").and_then(Value::as_array).into_iter().flatten() {
                let Some(url) = a.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) else { continue };
                let artifact = Artifact {
                    url: url.to_string(),
                    title: a.get("title").and_then(Value::as_str).map(str::to_string),
                    updated_at: a.get("updatedAt").and_then(Value::as_f64).map(|ms| ms / 1000.0),
                };
                let seen = artifacts.entry(part.acct.clone()).or_default();
                // The same artifact can be listed by several sessions: keep the newest word on it.
                match seen.get(url) {
                    Some(had) if had.updated_at >= artifact.updated_at => {}
                    _ => {
                        seen.insert(url.to_string(), artifact);
                    }
                }
            }
        }
    }
    let plugins = synced_plugins(ctx);
    let accounts: BTreeSet<&String> = connectors.keys().chain(plugins.keys()).collect();
    let accounts = accounts
        .into_iter()
        .map(|account| {
            let mut artifacts: Vec<Artifact> =
                artifacts.get(account).map(|a| a.values().cloned().collect()).unwrap_or_default();
            artifacts.sort_by(|a, b| b.updated_at.unwrap_or(0.0).total_cmp(&a.updated_at.unwrap_or(0.0)));
            Holdings {
                account: account.clone(),
                connectors: connectors.get(account).map(|c| c.values().cloned().collect()).unwrap_or_default(),
                plugins: plugins.get(account).map(|p| p.iter().cloned().collect()).unwrap_or_default(),
                artifacts,
            }
        })
        .collect();
    Inventory { accounts }
}

/// Plugins per account, from every `<org>_<account>` folder of Claude Code's synced plugins.
fn synced_plugins(ctx: &Ctx) -> BTreeMap<String, BTreeSet<Plugin>> {
    let mut out: BTreeMap<String, BTreeSet<Plugin>> = BTreeMap::new();
    let Ok(dirs) = fs::read_dir(&ctx.paths.synced_plugins) else { return out };
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        let Some((org, account)) = name.split_once('_') else { continue };
        if !is_uuid(org) || !is_uuid(account) || fsx::is_symlink(&dir.path()) {
            continue;
        }
        let have = out.entry(account.to_string()).or_default();
        let Ok(manifest) = fsx::read_json(&dir.path().join("manifest.json"), 4 * 1024 * 1024) else { continue };
        for p in manifest.get("plugins").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                let marketplace = p.get("marketplaceName").and_then(Value::as_str).map(str::to_string);
                have.insert(Plugin { name: name.to_string(), marketplace });
            }
        }
    }
    out
}
