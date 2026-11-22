// SPDX-License-Identifier: GPL-3.0-or-later
//! Failed logins at Asterisk (AMI security events: wrong password, unknown account, refused by ACL),
//! counted per hour for `/monitor/sip-auth` and Prometheus. Blocking (fail2ban, firewall) is the
//! deployment's business; Asterisk's security.log has the same events with all details.
//!
//! The events arrive on the call tracking connection (calltrack.rs, event mask `security`); the
//! chatty SuccessfulAuth/ChallengeSent are filtered out in manager.conf (site-config.sh).

use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
};

use crate::calltrack::Msg;

/// failures within this window count
const WINDOW_S: i64 = 3600;
/// WARN from this many failures per hour (a phone with an old password retries every few minutes)
pub const WARN_FAILURES: usize = 10;
/// CRIT from this many failures or this many different sources per hour (scan or guessing)
pub const CRIT_FAILURES: usize = 100;
pub const CRIT_SOURCES: usize = 10;
/// memory bound: oldest entries are dropped beyond this
const MAX_ENTRIES: usize = 20_000;

struct Failure {
    at: i64,
    source: String,
    account: String,
}

static LOG: Mutex<VecDeque<Failure>> = Mutex::new(VecDeque::new());

/// AMI security events that mean a failed login or a refused request.
fn is_failure(event: &str) -> bool {
    matches!(event, "InvalidPassword" | "ChallengeResponseFailed" | "InvalidAccountID" | "FailedACL" | "UnexpectedAddress" | "RequestNotAllowed")
}

/// "IPV4/UDP/203.0.113.5/5060" → "203.0.113.5"
fn source_ip(remote: &str) -> String {
    remote.split('/').nth(2).unwrap_or(remote).to_string()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Called for every AMI event of the call tracking connection.
pub fn observe(m: &Msg) {
    let ev = m.get("Event").map(String::as_str).unwrap_or("");
    if !is_failure(ev) {
        return;
    }
    let source = source_ip(m.get("RemoteAddress").map(String::as_str).unwrap_or(""));
    let account = m.get("AccountID").cloned().unwrap_or_default();
    let service = m.get("Service").map(String::as_str).unwrap_or("?");
    tracing::info!("failed login ({ev}, {service}) from {source} as {account:?}");
    record(Failure { at: now(), source, account });
}

fn record(f: Failure) {
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.push_back(f);
    while log.len() > MAX_ENTRIES {
        log.pop_front();
    }
}

/// Failures in the last hour: (count, distinct sources, the most frequent "source (n×, account)").
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub failures: usize,
    pub sources: usize,
    pub top: Vec<String>,
}

pub fn summary() -> Summary {
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let limit = now() - WINDOW_S;
    while log.front().is_some_and(|f| f.at < limit) {
        log.pop_front();
    }
    summarize(log.iter())
}

fn summarize<'a>(it: impl Iterator<Item = &'a Failure>) -> Summary {
    let mut per: Vec<(String, usize, HashSet<String>)> = vec![];
    let mut failures = 0;
    for f in it {
        failures += 1;
        match per.iter_mut().find(|p| p.0 == f.source) {
            Some(p) => {
                p.1 += 1;
                p.2.insert(f.account.clone());
            }
            None => per.push((f.source.clone(), 1, HashSet::from([f.account.clone()]))),
        }
    }
    let sources = per.len();
    per.sort_by(|a, b| b.1.cmp(&a.1));
    let top = per
        .iter()
        .take(3)
        .map(|(s, n, acc)| {
            let mut acc: Vec<&str> = acc.iter().map(String::as_str).filter(|a| !a.is_empty()).collect();
            acc.sort();
            let acc = if acc.len() > 3 { format!("{} accounts", acc.len()) } else { acc.join(", ") };
            format!("{s} ({n}×{})", if acc.is_empty() { String::new() } else { format!(", {acc}") })
        })
        .collect();
    Summary { failures, sources, top }
}

/// Checkmk state: 0 OK, 1 WARN, 2 CRIT.
pub fn state(s: &Summary) -> u8 {
    if s.failures >= CRIT_FAILURES || s.sources >= CRIT_SOURCES {
        2
    } else if s.failures >= WARN_FAILURES {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(src: &str, acc: &str) -> Failure {
        Failure { at: 0, source: src.into(), account: acc.into() }
    }

    #[test]
    fn source_and_events() {
        assert_eq!(source_ip("IPV4/UDP/203.0.113.5/5060"), "203.0.113.5");
        assert!(is_failure("ChallengeResponseFailed") && is_failure("InvalidAccountID"));
        assert!(!is_failure("SuccessfulAuth") && !is_failure("ChallengeSent"));
    }

    #[test]
    fn summary_and_state() {
        let v = vec![f("10.0.0.5", "dev1"), f("10.0.0.5", "dev1"), f("198.51.100.7", "100"), f("198.51.100.7", "101")];
        let s = summarize(v.iter());
        assert_eq!((s.failures, s.sources), (4, 2));
        assert_eq!(s.top[0], "10.0.0.5 (2×, dev1)");
        assert_eq!(state(&s), 0);
        let many: Vec<Failure> = (0..12).map(|i| f(&format!("192.0.2.{i}"), "x")).collect();
        assert_eq!(state(&summarize(many.iter())), 2, "many sources");
        let warn: Vec<Failure> = (0..10).map(|_| f("10.0.0.9", "old")).collect();
        assert_eq!(state(&summarize(warn.iter())), 1);
    }
}
