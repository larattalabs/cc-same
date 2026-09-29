//! What the window says, worked out from the engine's [`Overview`]: the headline, the accounts,
//! and how each ring in the headline sits. No drawing here; the words come from `locales/`.

use crate::i18n::{t, tf, tn};
use cc_same_core::report::{Overview, PartitionView, Warning};
use cc_same_core::{Surface, short};
use std::time::Duration;

/// How long a pending change may wait for the background agent before we offer "Sync now".
pub const AGENT_PATIENCE: Duration = Duration::from_secs(20);
/// With the agent running, this few sessions in flight is routine mirroring (the session you
/// are using right now, say), not worth a headline.
const ROUTINE_SESSIONS: usize = 3;

/// The state the headline reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Loading,
    /// No Claude data on this machine.
    Missing,
    /// Fewer than two accounts take part.
    Single,
    /// Another tool linked this many index folders; Claude stops saving there.
    Linked(usize),
    Syncing {
        sessions: usize,
    },
    Pending {
        sessions: usize,
        other: usize,
    },
    /// Updates that wait until Claude lets go of the account it has open.
    Waiting(usize),
    InSync {
        sessions: usize,
        accounts: usize,
    },
}

impl Mood {
    pub fn title(self) -> String {
        match self {
            Mood::Loading => t("headline.loading"),
            Mood::Missing => t("headline.missing.title"),
            Mood::Single => t("headline.single.title"),
            Mood::Linked(_) => t("headline.linked.title"),
            Mood::Syncing { .. } => t("headline.syncing.title"),
            Mood::Pending { sessions: 0, other } => tn("headline.pending.updates", other, &[]),
            Mood::Pending { sessions, .. } => tn("headline.pending.sessions", sessions, &[]),
            Mood::Waiting(_) => t("headline.waiting.title"),
            Mood::InSync { .. } => t("headline.in_sync.title"),
        }
    }

    pub fn detail(self) -> String {
        match self {
            Mood::Loading => String::new(),
            Mood::Missing => t("headline.missing.detail"),
            Mood::Single => t("headline.single.detail"),
            Mood::Linked(n) => tn("headline.linked.detail", n, &[]),
            Mood::Syncing { sessions: 0 } => t("headline.syncing.any"),
            Mood::Syncing { sessions } => tn("headline.syncing.detail", sessions, &[]),
            Mood::Pending { .. } => t("headline.pending.detail"),
            Mood::Waiting(n) => tn("headline.waiting.detail", n, &[]),
            Mood::InSync { sessions, accounts } => tn("headline.in_sync.detail", sessions, &[("accounts", &accounts)]),
        }
    }

    /// Whether every account has everything.
    pub fn settled(self) -> bool {
        matches!(self, Mood::InSync { .. })
    }

    /// Whether the user has something to do.
    pub fn needs_attention(self) -> bool {
        matches!(self, Mood::Linked(_) | Mood::Pending { .. } | Mood::Missing)
    }
}

/// Work in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Busy {
    Syncing,
    Service,
    Restoring,
    Fixing,
    Saving,
}

/// One session list Claude shows: an account in one organization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// `<account>/<org>`, unique per row.
    pub key: String,
    pub id: String,
    /// The email, or `Account 1a2b3c4d` when we do not know it.
    pub name: String,
    pub has_email: bool,
    /// Which organization, when the account has more than one.
    pub org: Option<String>,
    pub sessions: usize,
    /// Sessions the others have and this one does not, yet.
    pub missing: usize,
    /// Claude has this one open.
    pub open: bool,
    pub excluded: bool,
    /// The folder is a link; Claude cannot save here.
    pub broken: bool,
}

impl Account {
    /// One or two characters for the ring and the avatar.
    pub fn initials(&self) -> String {
        if self.has_email {
            self.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
        } else {
            self.id.chars().take(2).collect::<String>().to_uppercase()
        }
    }

    pub fn detail(&self) -> String {
        if self.broken {
            return t("account.broken");
        }
        if self.excluded {
            return t("account.excluded");
        }
        let mut parts = vec![tn("account.sessions", self.sessions, &[])];
        if self.missing > 0 {
            parts.push(tf("account.to_copy", &[("count", &self.missing)]));
        }
        if let Some(org) = &self.org {
            parts.push(org.clone());
        }
        parts.join(" · ")
    }

    pub fn ring(&self) -> Placement {
        if self.broken {
            Placement::Broken
        } else if self.excluded {
            Placement::Apart
        } else if self.missing > 0 {
            Placement::Behind
        } else {
            Placement::Joined
        }
    }
}

/// Where a ring sits relative to the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Has everything: overlaps the others fully.
    Joined,
    /// Still missing sessions: drifts out a little until it catches up.
    Behind,
    /// Excluded from the sync: set apart and faded.
    Apart,
    /// Claude cannot save to it.
    Broken,
}

/// One row per Code index (the list Claude shows), labelled with an email when we know it.
pub fn accounts(ov: &Overview) -> Vec<Account> {
    let Some(code) = ov.surface(Surface::Code) else { return Vec::new() };
    let orgs = |acct: &str| code.partitions.iter().filter(|p| p.part.acct == acct).count();
    let mut rows: Vec<Account> = code
        .partitions
        .iter()
        .map(|p: &PartitionView| Account {
            key: format!("{}/{}", p.part.acct, p.part.org),
            id: p.part.acct.clone(),
            name: p.email.clone().unwrap_or_else(|| tf("account.unnamed", &[("id", &short(&p.part.acct))])),
            has_email: p.email.is_some(),
            org: (orgs(&p.part.acct) > 1).then(|| tf("account.org", &[("id", &short(&p.part.org))])),
            sessions: p.sessions,
            missing: if p.excluded { 0 } else { p.missing },
            open: p.loaded,
            excluded: p.excluded,
            broken: p.error.is_some() || p.part.is_link,
        })
        .collect();
    // Named accounts first, then the ones we only know by id; excluded ones last.
    rows.sort_by(|a, b| {
        let key = |r: &Account| (r.excluded, !r.has_email, r.name.to_lowercase(), r.key.clone());
        key(a).cmp(&key(b))
    });
    rows
}

pub fn agent_alive(ov: &Overview) -> bool {
    ov.service.installed && (ov.heartbeat.as_ref().is_some_and(|h| h.is_fresh()) || ov.service.running == Some(true))
}

/// The headline. `pending_for` is how long the current plan has been waiting.
pub fn mood(ov: Option<&Overview>, syncing: bool, pending_for: Option<Duration>) -> Mood {
    let Some(ov) = ov else { return Mood::Loading };
    if syncing {
        return Mood::Syncing { sessions: ov.plan.sessions };
    }
    if ov.warnings.iter().any(|w| matches!(w, Warning::DesktopDataMissing { .. })) {
        return Mood::Missing;
    }
    if let Some(n) = ov.warnings.iter().find_map(|w| match w {
        Warning::SymlinkedFolders { count, .. } => Some(*count),
        _ => None,
    }) {
        return Mood::Linked(n);
    }
    let accounts = accounts(ov);
    let taking_part = accounts.iter().filter(|a| !a.excluded && !a.broken).count();
    if taking_part < 2 {
        return Mood::Single;
    }
    let union = ov.surface(Surface::Code).map(|s| s.union).unwrap_or(0);
    if ov.plan.total > 0 {
        let sessions = ov.plan.sessions;
        let fresh = pending_for.is_none_or(|t| t < AGENT_PATIENCE);
        if agent_alive(ov) && fresh {
            // The agent is on it; a handful in flight is just mirroring.
            return if sessions <= ROUTINE_SESSIONS {
                Mood::InSync { sessions: union, accounts: taking_part }
            } else {
                Mood::Syncing { sessions }
            };
        }
        let writes: usize = ov
            .plan
            .per_partition
            .iter()
            .flat_map(|(_, kinds)| kinds.iter())
            .filter(|(k, _)| k.ends_with("sessions"))
            .map(|(_, n)| *n)
            .sum();
        return Mood::Pending { sessions, other: ov.plan.total.saturating_sub(writes) };
    }
    let deferred: usize = ov.plan.deferred.values().sum();
    if deferred > 0 { Mood::Waiting(deferred) } else { Mood::InSync { sessions: union, accounts: taking_part } }
}

/// `20260928-225026-baseline` → `Sep 28, 22:50` in the interface language.
pub fn snapshot_when(id: &str) -> String {
    let (Some(m), Some(d), Some(hh), Some(mm)) = (id.get(4..6), id.get(6..8), id.get(9..11), id.get(11..13)) else {
        return id.to_string();
    };
    match (m.parse::<usize>(), d.parse::<u32>()) {
        (Ok(m), Ok(d)) if (1..=12).contains(&m) => crate::i18n::short_date(m, d, &format!("{hh}:{mm}")),
        _ => id.to_string(),
    }
}

pub fn snapshot_reason(reason: &str, baseline: bool) -> String {
    t(if baseline {
        "snapshot.first"
    } else {
        match reason {
            "baseline" => "snapshot.first",
            "pre-restore" => "snapshot.pre_restore",
            "watch" => "snapshot.automatic",
            _ => "snapshot.manual",
        }
    })
}

/// How long transcripts are kept, for display.
pub fn retention_label(days: Option<f64>) -> String {
    match days {
        Some(d) if d >= 3650.0 => tn("period.years", (d / 365.0).round() as usize, &[]),
        Some(d) => tn("period.days", d as usize, &[]),
        None => tn("period.days", 30, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, has_email: bool) -> Account {
        Account {
            key: format!("{name}/org"),
            id: "9e31ea7e-0000-0000-0000-000000000000".into(),
            name: name.into(),
            has_email,
            org: None,
            sessions: 209,
            missing: 0,
            open: false,
            excluded: false,
            broken: false,
        }
    }

    #[test]
    fn headline_copy() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        assert_eq!(Mood::InSync { sessions: 209, accounts: 3 }.detail(), "209 sessions in each of your 3 accounts");
        assert_eq!(Mood::Pending { sessions: 1, other: 0 }.title(), "1 session to copy");
        assert_eq!(Mood::Pending { sessions: 0, other: 2 }.title(), "2 updates to apply");
        assert_eq!(Mood::Waiting(1).detail(), "1 update lands when you switch accounts or quit Claude.");
        assert_eq!(Mood::Waiting(4).detail(), "4 updates land when you switch accounts or quit Claude.");
    }

    #[test]
    fn rows_describe_themselves() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let mut a = account("i@example.com", true);
        assert_eq!(a.initials(), "I");
        assert_eq!(a.detail(), "209 sessions");
        assert_eq!(a.ring(), Placement::Joined);
        a.missing = 3;
        assert_eq!(a.detail(), "209 sessions · 3 to copy");
        assert_eq!(a.ring(), Placement::Behind);
        a.excluded = true;
        assert_eq!((a.detail().as_str(), a.ring()), ("Kept separate", Placement::Apart));
        let b = account("Account 9e31ea7e", false);
        assert_eq!(b.initials(), "9E");
    }

    #[test]
    fn snapshot_labels() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        assert_eq!(snapshot_when("20260928-225026-baseline"), "Sep 28, 22:50");
        assert_eq!(snapshot_when("garbage"), "garbage");
        assert_eq!(snapshot_reason("watch", false), "Automatic");
        assert_eq!(snapshot_reason("watch", true), "Before the first sync");
        assert_eq!(retention_label(Some(3650.0)), "10 years");
        assert_eq!(retention_label(None), "30 days");
    }
}
