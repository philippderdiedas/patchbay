// SPDX-License-Identifier: GPL-3.0-or-later
//! Call flow diagrams ("Ablauf anzeigen"): what happens to a call on an external number or an
//! internal number, in plain words, for people who do not read dialplans.
//!
//! The configuration is loaded once (small tables) and walked synchronously, mirroring the dialplan
//! (extensions.conf: from-sip-trunk, sub-target, sub-user, sub-group, sub-ivr). Keep both in sync.
//! External numbers are DIDs of SIP trunks in DID mode; passthrough trunks route by their trunk
//! inbound rules (trunk-in, shown on the trunk's page), not by DIDs.
//! Live state (time profile now, DND, logged out, registered phones) is shown as notes, the
//! structure itself is the configuration.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::{
    extract::{Path, State},
    response::Response,
};
use chrono::NaiveTime;
use sqlx::{FromRow, PgPool};

use crate::{AppError, AppState, ami, auth::Admin, i18n::Tr, ui::{Ctx, render}};

// --- Diagram -------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Node {
    /// CSS kind: start, time, person, group, ivr, mailbox, sound, external, end, ref, warn, app
    class: &'static str,
    title: String,
    lines: Vec<String>,
    /// live state notes: (is a warning, text)
    live: Vec<(bool, String)>,
    edges: Vec<Edge>,
}

#[derive(Debug)]
pub struct Edge {
    label: String,
    /// the branch that applies right now (time profile, forward)
    active: bool,
    node: Node,
}

impl Node {
    fn new(class: &'static str, title: impl Into<String>) -> Self {
        Node { class, title: title.into(), ..Default::default() }
    }
    fn line(mut self, s: impl Into<String>) -> Self {
        self.lines.push(s.into());
        self
    }
    fn edge(&mut self, label: impl Into<String>, active: bool, node: Node) {
        self.edges.push(Edge { label: label.into(), active, node });
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Nested HTML (all text escaped): a box per step, the branches below it.
pub fn html(n: &Node, t: &Tr) -> String {
    let mut o = String::new();
    write_node(&mut o, n, t);
    o
}

fn write_node(o: &mut String, n: &Node, t: &Tr) {
    o.push_str(&format!("<div class=\"fn fn-{}\"><div class=\"fn-title\">{}</div>", n.class, esc(&n.title)));
    if !n.lines.is_empty() {
        o.push_str("<ul class=\"fn-lines\">");
        for l in &n.lines {
            // "• name" / "2. name": marker hanging in front of wrapped lines
            let mark = l.strip_prefix("• ").map(|r| ("•".to_string(), r)).or_else(|| {
                let (num, rest) = l.split_once(". ")?;
                (!num.is_empty() && num.len() <= 3 && num.chars().all(|c| c.is_ascii_digit())).then(|| (format!("{num}."), rest))
            });
            match mark {
                Some((m, rest)) => o.push_str(&format!("<li class=\"fn-item\" data-mark=\"{}\">{}</li>", esc(&m), esc(rest))),
                None => o.push_str(&format!("<li>{}</li>", esc(l))),
            }
        }
        o.push_str("</ul>");
    }
    for (warn, l) in &n.live {
        o.push_str(&format!("<div class=\"fn-live{}\">{}</div>", if *warn { " warn" } else { "" }, esc(l)));
    }
    o.push_str("</div>");
    if !n.edges.is_empty() {
        o.push_str("<ul class=\"fn-edges\">");
        for e in &n.edges {
            o.push_str("<li>");
            if !e.label.is_empty() || e.active {
                o.push_str(&format!("<div class=\"fn-edge{}\">{}", if e.active { " active" } else { "" }, esc(&e.label)));
                if e.active {
                    o.push_str(&format!(" <span class=\"fn-now\">{}</span>", esc(t.s("applies now"))));
                }
                o.push_str("</div>");
            }
            write_node(o, &e.node, t);
            o.push_str("</li>");
        }
        o.push_str("</ul>");
    }
}

/// "{}" placeholders filled in order (translated template first).
pub(crate) fn tf(t: &Tr, key: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::new();
    let mut rest = t.s(key);
    for a in args {
        match rest.find("{}") {
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str(&a.to_string());
                rest = &rest[i + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

// --- Configuration snapshot ------------------------------------------------------------------

#[derive(FromRow, Clone, Default)]
pub struct TargetRow {
    id: i32,
    kind: String,
    user_id: Option<i32>,
    group_id: Option<i32>,
    ivr_id: Option<i32>,
    sound_id: Option<i32>,
    external_e164: Option<String>,
    owner_user_id: Option<i32>,
    app_id: Option<i32>,
}

#[derive(FromRow, Clone, Default)]
pub struct UserRow {
    id: i32,
    extension: i16,
    display_name: String,
    active: bool,
    dnd: bool,
    ring_timeout_s: i16,
    forward_immediate: Option<i32>,
    no_answer_target: Option<i32>,
    /// numbers on the person's own blocklist
    blocked: i64,
    /// outbound number (users.outbound_did_id) and the trunk it belongs to
    out_e164: Option<String>,
    out_trunk: Option<String>,
    /// routetest::NEVER_SHOWN
    out_never_shown: bool,
}

#[derive(FromRow, Clone, Default)]
pub struct DeviceRow {
    user_id: i32,
    label: String,
    sip_username: String,
    ring: bool,
    ring_delay_s: i16,
}

#[derive(FromRow, Clone, Default)]
pub struct GroupRow {
    id: i32,
    number: i16,
    name: String,
    strategy: String,
    member_timeout_s: i16,
    total_timeout_s: i16,
    moh: bool,
    no_answer_target: Option<i32>,
    caller_name_prefix: String,
    queue: bool,
    queue_announce_s: i16,
    announce_sound_id: Option<i32>,
}

#[derive(FromRow, Clone, Default)]
pub struct MemberRow {
    group_id: i32,
    user_id: i32,
    enabled: bool,
    logged_in: bool,
}

#[derive(FromRow, Clone, Default)]
pub struct IvrRow {
    id: i32,
    name: String,
    greeting: Option<i32>,
    timeout_s: i16,
    repeat: i16,
    timeout_target: Option<i32>,
    invalid_mode: String,
}

#[derive(FromRow, Clone, Default)]
pub struct DidRow {
    id: i32,
    e164: String,
    label: String,
    time_profile_id: Option<i32>,
    target_open: Option<i32>,
    target_closed: Option<i32>,
    target_holiday: Option<i32>,
}

#[derive(FromRow, Clone)]
pub struct RuleRow {
    profile_id: i32,
    weekdays: i16,
    from_time: NaiveTime,
    to_time: NaiveTime,
}

#[derive(Default)]
pub struct Config {
    targets: HashMap<i32, TargetRow>,
    users: HashMap<i32, UserRow>,
    devices: Vec<DeviceRow>,
    groups: HashMap<i32, GroupRow>,
    /// ordered like GROUP_MEMBERS (priority, extension)
    members: Vec<MemberRow>,
    ivrs: HashMap<i32, IvrRow>,
    /// (ivr id, digit, target id)
    ivr_options: Vec<(i32, String, i32)>,
    sounds: HashMap<i32, String>,
    /// (owner type "user"/"group", owner id) with an own mailbox greeting
    greetings: HashSet<(String, i32)>,
    /// id -> (name, state now: open/closed/holiday)
    profiles: HashMap<i32, (String, String)>,
    rules: Vec<RuleRow>,
    dids: HashMap<i32, DidRow>,
    /// internal number -> (kind, ref id)
    numbers: HashMap<i16, (String, i32)>,
    /// app id -> (name, kind, active: enabled and in app_dialplan, i.e. what sub-app finds)
    apps: HashMap<i32, (String, String, bool)>,
    /// custom app id -> (name, enabled)
    custom_apps: HashMap<i32, (String, bool)>,
    blocked: i64,
    block_anonymous: bool,
    recall_s: i32,
    /// registered devices (SIP usernames); None: Asterisk not reachable, no device notes
    registered: Option<HashSet<String>>,
}

async fn all<T: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin>(db: &PgPool, sql: &str) -> anyhow::Result<Vec<T>> {
    Ok(sqlx::query_as(sql).fetch_all(db).await?)
}

impl Config {
    pub async fn load(db: &PgPool) -> anyhow::Result<Config> {
        let mut c = Config::default();
        for r in all::<TargetRow>(db, "SELECT id, kind::text, user_id, group_id, ivr_id, sound_id, external_e164, owner_user_id, app_id FROM targets").await? {
            c.targets.insert(r.id, r);
        }
        for r in all::<UserRow>(
            db,
            &format!(
                "SELECT u.id, u.extension, u.display_name, (u.enabled AND u.phone_allowed) AS active, u.dnd, u.ring_timeout_s,
                        u.forward_immediate, u.no_answer_target,
                        (SELECT count(*) FROM user_blocked_numbers b WHERE b.user_id = u.id) AS blocked,
                        d.e164 AS out_e164, t.name AS out_trunk, COALESCE({}, false) AS out_never_shown
                 FROM users u LEFT JOIN dids d ON d.id = u.outbound_did_id LEFT JOIN trunks t ON t.id = d.trunk_id
                 WHERE u.deleted_at IS NULL",
                crate::routetest::NEVER_SHOWN
            ),
        )
        .await?
        {
            c.users.insert(r.id, r);
        }
        c.devices = all(db, "SELECT user_id, label, sip_username, ring, ring_delay_s FROM active_devices ORDER BY ring_order, id").await?;
        for r in all::<GroupRow>(
            db,
            "SELECT id, number, name, strategy, member_timeout_s, total_timeout_s, moh, no_answer_target, caller_name_prefix,
                    queue, queue_announce_s, announce_sound_id FROM groups WHERE deleted_at IS NULL",
        )
        .await?
        {
            c.groups.insert(r.id, r);
        }
        c.members = all(
            db,
            "SELECT m.group_id, m.user_id, m.enabled, m.logged_in FROM group_members m JOIN users u ON u.id = m.user_id
             WHERE u.deleted_at IS NULL ORDER BY m.priority, u.extension",
        )
        .await?;
        for r in all::<IvrRow>(db, "SELECT id, name, greeting, timeout_s, repeat, timeout_target, invalid_mode FROM ivrs WHERE deleted_at IS NULL").await? {
            c.ivrs.insert(r.id, r);
        }
        c.ivr_options = sqlx::query_as("SELECT ivr_id, digit, target_id FROM ivr_options ORDER BY ivr_id, digit").fetch_all(db).await?;
        let sounds: Vec<(i32, String)> = sqlx::query_as("SELECT id, name FROM sounds").fetch_all(db).await?;
        c.sounds = sounds.into_iter().collect();
        let gr: Vec<(String, i32)> = sqlx::query_as(
            "SELECT g.owner_type::text, g.owner_id FROM greetings g JOIN sounds s ON s.id = g.sound_id
             WHERE g.kind = 'unavailable' AND s.deleted_at IS NULL AND s.file_path <> ''",
        )
        .fetch_all(db)
        .await?;
        c.greetings = gr.into_iter().collect();
        let profiles: Vec<(i32, String, String)> =
            sqlx::query_as("SELECT id, name, time_profile_state(id) FROM time_profiles WHERE deleted_at IS NULL").fetch_all(db).await?;
        c.profiles = profiles.into_iter().map(|(id, n, s)| (id, (n, s))).collect();
        c.rules = all(db, "SELECT profile_id, weekdays, from_time, to_time FROM time_rules ORDER BY profile_id, from_time").await?;
        for r in all::<DidRow>(
            db,
            "SELECT d.id, d.e164, d.label, ir.time_profile_id, ir.target_open, ir.target_closed, ir.target_holiday
             FROM dids d LEFT JOIN inbound_rules ir ON ir.did_id = d.id",
        )
        .await?
        {
            c.dids.insert(r.id, r);
        }
        let numbers: Vec<(i16, String, i32)> = sqlx::query_as("SELECT number, kind::text, ref_id FROM numbers").fetch_all(db).await?;
        c.numbers = numbers.into_iter().map(|(n, k, r)| (n, (k, r))).collect();
        let apps: Vec<(i32, String, String, bool)> = sqlx::query_as(
            "SELECT id, name, kind, enabled AND id IN (SELECT id FROM app_dialplan) FROM apps WHERE deleted_at IS NULL",
        )
        .fetch_all(db)
        .await?;
        c.apps = apps.into_iter().map(|(id, n, k, a)| (id, (n, k, a))).collect();
        let custom: Vec<(i32, String, bool)> = sqlx::query_as("SELECT id, name, enabled FROM custom_apps WHERE deleted_at IS NULL").fetch_all(db).await?;
        c.custom_apps = custom.into_iter().map(|(id, n, e)| (id, (n, e))).collect();
        c.blocked = sqlx::query_scalar("SELECT count(*) FROM blocked_numbers").fetch_one(db).await?;
        let s: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM settings WHERE key IN ('block_anonymous', 'transfer_recall_s')").fetch_all(db).await?;
        for (k, v) in s {
            match k.as_str() {
                "block_anonymous" => c.block_anonymous = v == "true",
                _ => c.recall_s = v.parse().unwrap_or(0),
            }
        }
        c.registered = ami::contacts().await.ok().map(|m| m.into_keys().collect());
        Ok(c)
    }
}

// --- Walk ------------------------------------------------------------------------------------

const MAX_NODES: usize = 250;

struct Walk<'a> {
    c: &'a Config,
    t: &'a Tr,
    /// call path (loop detection): "u:<id>", "g:<id>", "i:<id>" -> title
    path: Vec<(String, String)>,
    nodes: usize,
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

impl<'a> Walk<'a> {
    fn s(&self, k: &'static str) -> &'static str {
        self.t.s(k)
    }

    fn end(&self, text: &'static str) -> Node {
        Node::new("end", self.s(text))
    }

    fn person_name(&self, id: i32) -> String {
        self.c.users.get(&id).map(|u| format!("{} ({})", u.display_name, u.extension)).unwrap_or_else(|| "?".into())
    }

    fn group_name(&self, id: i32) -> String {
        self.c.groups.get(&id).map(|g| format!("{} ({})", g.name, g.number)).unwrap_or_else(|| "?".into())
    }

    /// Loop check: Some(node) when `key` is already on the call path.
    fn enter(&mut self, key: String, title: &str) -> Option<Node> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Some(Node::new("warn", self.s("… (diagram too large, shortened)")));
        }
        if let Some((_, t)) = self.path.iter().find(|(k, _)| *k == key) {
            return Some(
                Node::new("ref", tf(self.t, "back to {} (loop)", &[t]))
                    .line(self.s("Forwards are followed at most 3 times, then the caller hears “nobody available”.")),
            );
        }
        self.path.push((key, title.to_string()));
        None
    }

    fn leave(&mut self) {
        self.path.pop();
    }

    /// sub-app (or a custom app's dialplan block): (name, kind, active); None = app missing
    fn app(&self, app: Option<(String, String, bool)>) -> Node {
        let Some((name, kind, active)) = app else { return self.end("Target missing") };
        let n = Node::new("app", tf(self.t, "App “{}”", &[&name]));
        if active {
            n.line(self.s(crate::apps::kind_help(&kind)))
        } else {
            n.line(self.s("Not active (switched off or not allowed at this site): announcement “invalid”."))
        }
    }

    /// sub-target
    fn target(&mut self, id: Option<i32>) -> Node {
        let Some(t) = id.and_then(|id| self.c.targets.get(&id)).cloned() else {
            return self.end("No target: announcement “service not available”, then hang up");
        };
        match t.kind.as_str() {
            "user" => t.user_id.map(|u| self.person(u)).unwrap_or_else(|| self.end("Target missing")),
            "group" => t.group_id.map(|g| self.group(g)).unwrap_or_else(|| self.end("Target missing")),
            "ivr" => t.ivr_id.map(|i| self.ivr(i)).unwrap_or_else(|| self.end("Target missing")),
            "mailbox_user" => self.mailbox("user", t.user_id.unwrap_or(0)),
            "mailbox_group" => self.mailbox("group", t.group_id.unwrap_or(0)),
            "app" => {
                let app = t.app_id.and_then(|a| self.c.apps.get(&a)).cloned();
                self.app(app)
            }
            "announcement" => {
                let name = t.sound_id.and_then(|s| self.c.sounds.get(&s)).cloned().unwrap_or_else(|| "?".into());
                Node::new("sound", tf(self.t, "Announcement “{}”", &[&name])).line(self.s("Then the call ends."))
            }
            "external" => {
                let mut n = Node::new("external", tf(self.t, "Forward to {}", &[&t.external_e164.clone().unwrap_or_default()]));
                if let Some(o) = t.owner_user_id {
                    n = n.line(tf(self.t, "Set up by {}, dialled with their permissions", &[&self.person_name(o)]));
                }
                n.line(self.s("Goes out over a trunk line, from there the other side decides."))
            }
            "hangup" => self.end("Hang up"),
            _ => self.end("No target: announcement “service not available”, then hang up"),
        }
    }

    fn no_answer(&mut self, target: Option<i32>) -> Node {
        match target {
            Some(_) => self.target(target),
            None => self.end("Announcement “nobody available”, then hang up"),
        }
    }

    fn ring_devices(&self, user_id: i32) -> Vec<&'a DeviceRow> {
        self.c.devices.iter().filter(|d| d.user_id == user_id && d.ring).collect()
    }

    fn connected(&self, devs: &[&DeviceRow]) -> Option<bool> {
        self.c.registered.as_ref().map(|r| devs.iter().any(|d| r.contains(&d.sip_username)))
    }

    /// sub-user
    fn person(&mut self, uid: i32) -> Node {
        let Some(u) = self.c.users.get(&uid).cloned() else { return self.end("Target missing") };
        let title = format!("{} ({})", u.display_name, u.extension);
        if let Some(n) = self.enter(format!("u:{uid}"), &title) {
            return n;
        }
        let mut n = Node::new("person", title);
        if !u.active {
            n = n.line(self.s("Not active (no phone permission): announcement “invalid”."));
            self.leave();
            return n;
        }
        // sub-outbound / route_trunk_order(): the trunk of the own number is tried first
        if let (Some(e164), Some(trunk)) = (&u.out_e164, &u.out_trunk) {
            n = n.line(tf(self.t, "Own calls out: preferably over {} with the number {} (where a dial rule lists it and shows the person's number).", &[trunk, e164]));
            if u.out_never_shown {
                n.live.push((true, tf(self.t, crate::routetest::NEVER_SHOWN_TEXT, &[e164, trunk])));
            }
        }
        if u.blocked > 0 {
            n = n.line(tf(self.t, "Callers on the personal blocklist ({} numbers) hear busy.", &[&u.blocked]));
        }
        if let Some(fwd) = u.forward_immediate {
            n = n.line(self.s("Immediate forward is on: the phones do not ring."));
            let next = self.target(Some(fwd));
            n.edge(self.s("forwarded right away"), true, next);
            self.leave();
            return n;
        }
        let devs = self.ring_devices(uid);
        if devs.is_empty() {
            n = n.line(self.s("No phone set up to ring."));
        } else {
            n = n.line(tf(self.t, "Rings for {} s on:", &[&u.ring_timeout_s]));
            for d in &devs {
                n = n.line(if d.ring_delay_s > 0 {
                    format!("• {} {}", d.label, tf(self.t, "(only after {} s)", &[&d.ring_delay_s]))
                } else {
                    format!("• {}", d.label)
                });
            }
        }
        if u.dnd {
            n.live.push((true, self.s("Do not disturb is on: does not ring, goes straight on.").into()));
        } else if !devs.is_empty() && self.connected(&devs) == Some(false) {
            n.live.push((true, self.s("No phone connected right now: goes straight on.").into()));
        }
        let next = self.no_answer(u.no_answer_target);
        n.edge(self.s("not answered, busy or not reachable"), false, next);
        self.leave();
        n
    }

    /// Why a member does not ring right now (group_ringing_members), None = rings.
    fn member_skip(&self, m: &MemberRow) -> Option<&'static str> {
        let u = self.c.users.get(&m.user_id)?;
        if !m.enabled {
            return Some("switched off in this group");
        }
        if !u.active {
            return Some("no phone permission");
        }
        if !m.logged_in {
            return Some("logged out");
        }
        if u.dnd {
            return Some("do not disturb");
        }
        let devs = self.ring_devices(u.id);
        if devs.is_empty() {
            return Some("no phone set up");
        }
        if self.connected(&devs) == Some(false) {
            return Some("no phone connected");
        }
        None
    }

    /// sub-group
    fn group(&mut self, gid: i32) -> Node {
        let Some(g) = self.c.groups.get(&gid).cloned() else { return self.end("Target missing") };
        let title = tf(self.t, "Group {}", &[&format!("{} ({})", g.name, g.number)]);
        if let Some(n) = self.enter(format!("g:{gid}"), &title) {
            return n;
        }
        let mut n = Node::new("group", title);
        if let Some(s) = g.announce_sound_id {
            let name = self.c.sounds.get(&s).cloned().unwrap_or_else(|| "?".into());
            n = n.line(tf(self.t, "First the announcement “{}”", &[&name]));
        }
        if g.queue {
            n = n.line(self.s("Queue: callers wait in line, only free members ring."));
            if g.queue_announce_s > 0 {
                n = n.line(tf(self.t, "Callers hear their position (again every {} s when it improves).", &[&g.queue_announce_s]));
            }
        }
        n = n.line(match g.strategy.as_str() {
            "linear" => tf(self.t, "One after another in this order, {} s each, at most {} s in total.", &[&g.member_timeout_s, &g.total_timeout_s]),
            "rrmemory" => tf(self.t, "In turn (the start moves on with every call), {} s each, at most {} s in total.", &[
                &g.member_timeout_s,
                &g.total_timeout_s,
            ]),
            _ => tf(self.t, "All ring at the same time, for up to {} s.", &[&g.total_timeout_s]),
        });
        n = n.line(if g.moh { self.s("The caller hears hold music.") } else { self.s("The caller hears the ringing tone.") });
        if !g.caller_name_prefix.is_empty() {
            n = n.line(tf(self.t, "Display on the phones: “{} …” in front of the caller.", &[&g.caller_name_prefix]));
        }
        let members: Vec<MemberRow> = self.c.members.iter().filter(|m| m.group_id == gid).cloned().collect();
        if members.is_empty() {
            n = n.line(self.s("No members."));
        }
        let mut ringing = 0;
        for (i, m) in members.iter().enumerate() {
            let name = self.person_name(m.user_id);
            let label = if g.strategy == "ringall" { format!("• {name}") } else { format!("{}. {name}", i + 1) };
            match self.member_skip(m) {
                None => {
                    ringing += 1;
                    n = n.line(label);
                }
                Some(why) => n = n.line(format!("{label} – {} ({})", self.s("skipped right now"), self.s(why))),
            }
        }
        n = n.line(self.s("Personal forwards of the members do not apply here."));
        if ringing == 0 && !members.is_empty() {
            n.live.push((true, self.s("Nobody would ring right now: goes straight on.").into()));
        }
        let next = self.no_answer(g.no_answer_target);
        n.edge(self.s("nobody answers"), false, next);
        self.leave();
        n
    }

    /// sub-ivr
    fn ivr(&mut self, iid: i32) -> Node {
        let Some(v) = self.c.ivrs.get(&iid).cloned() else { return self.end("Target missing") };
        let title = tf(self.t, "Voice menu “{}”", &[&v.name]);
        if let Some(n) = self.enter(format!("i:{iid}"), &title) {
            return n;
        }
        let mut n = Node::new("ivr", title);
        n = n.line(match v.greeting.and_then(|s| self.c.sounds.get(&s)) {
            Some(name) => tf(self.t, "Plays “{}”", &[name]),
            None => self.s("No greeting (silence)").to_string(),
        });
        n = n.line(if v.repeat > 0 {
            tf(self.t, "Waits {} s for a key, repeats up to {} times.", &[&v.timeout_s, &v.repeat])
        } else {
            tf(self.t, "Waits {} s for a key, no repeat.", &[&v.timeout_s])
        });
        n = n.line(if v.invalid_mode == "ignore" {
            self.s("Other keys are ignored.")
        } else {
            self.s("Wrong key: “invalid”, then the menu again.")
        });
        let opts: Vec<(String, i32)> = self.c.ivr_options.iter().filter(|o| o.0 == iid).map(|o| (o.1.clone(), o.2)).collect();
        for (digit, tid) in opts {
            let next = self.target(Some(tid));
            n.edge(tf(self.t, "key {}", &[&digit]), false, next);
        }
        let next = match v.timeout_target {
            Some(_) => self.target(v.timeout_target),
            None => self.end("Hang up"),
        };
        n.edge(self.s("no key pressed"), false, next);
        self.leave();
        n
    }

    fn mailbox(&mut self, owner: &str, id: i32) -> Node {
        let name = if owner == "user" { self.person_name(id) } else { self.group_name(id) };
        let own = self.c.greetings.contains(&(owner.to_string(), id));
        Node::new("mailbox", tf(self.t, "Mailbox of {}", &[&name])).line(if own {
            self.s("Own greeting, then the caller can leave a message (up to 3 min).")
        } else {
            self.s("Standard greeting, then the caller can leave a message (up to 3 min).")
        })
    }

    fn profile_rules(&self, pid: i32) -> Vec<String> {
        self.c
            .rules
            .iter()
            .filter(|r| r.profile_id == pid)
            .map(|r| {
                let days: Vec<&str> = (0..7).filter(|i| r.weekdays >> i & 1 == 1).map(|i| self.t.s(DAYS[i as usize])).collect();
                format!("{} {}–{}", days.join(", "), r.from_time.format("%H:%M"), r.to_time.format("%H:%M"))
            })
            .collect()
    }

    /// from-sip-trunk for one DID. The admin blocklist comes first, as in the dialplan, where it runs
    /// for every SIP trunk (passthrough trunks too, before trunk-in) but not for IAX2 peers.
    fn did(&mut self, d: &DidRow) -> Node {
        let title = if d.label.is_empty() { tf(self.t, "Call to {}", &[&d.e164]) } else { tf(self.t, "Call to {}", &[&format!("{} ({})", d.label, d.e164)]) };
        let mut n = Node::new("start", title);
        if self.c.blocked > 0 {
            n = n.line(tf(self.t, "Callers on the blocklist ({} entries) hear busy.", &[&self.c.blocked]));
        }
        if self.c.block_anonymous {
            n = n.line(self.s("Callers without a number hear busy."));
        }
        n = n.line(self.s("Up to 30 minutes after an emergency call over this line, the emergency caller's phones ring first (25 s)."));
        let Some(open) = d.target_open else {
            let e = self.end("No target: announcement “service not available”, then hang up");
            n.edge("", false, e);
            return n;
        };
        let Some(pid) = d.time_profile_id else {
            let next = self.target(Some(open));
            n.edge("", false, next);
            return n;
        };
        let (pname, state) = self.c.profiles.get(&pid).cloned().unwrap_or_else(|| ("?".into(), "closed".into()));
        let mut tp = Node::new("time", tf(self.t, "Opening hours “{}”", &[&pname]));
        let rules = self.profile_rules(pid);
        if rules.is_empty() {
            tp = tp.line(self.s("No opening times entered: always closed."));
        }
        for r in rules {
            tp = tp.line(r);
        }
        let open_node = self.target(Some(open));
        tp.edge(self.s("open"), state == "open", open_node);
        let closed_node = match d.target_closed {
            Some(c) if c != open => self.target(Some(c)),
            _ => Node::new("ref", self.s("same as “open”")),
        };
        tp.edge(self.s("closed"), state == "closed", closed_node);
        let holiday_node = match d.target_holiday {
            Some(h) if Some(h) != d.target_closed && h != open => self.target(Some(h)),
            _ if d.target_closed.is_some_and(|c| c != open) => Node::new("ref", self.s("same as “closed”")),
            _ => Node::new("ref", self.s("same as “open”")),
        };
        tp.edge(self.s("holiday or closing day"), state == "holiday", holiday_node);
        n.edge("", false, tp);
        n
    }

    /// sub-internal for an internal number
    fn internal(&mut self, number: i16) -> Option<Node> {
        let (kind, rid) = self.c.numbers.get(&number).cloned()?;
        let mut start = Node::new("start", tf(self.t, "Internal call to {}", &[&number]));
        let next = match kind.as_str() {
            "person" => self.person(rid),
            "group" => self.group(rid),
            "ivr" => self.ivr(rid),
            "device" => Node::new("person", self.s("Rings exactly this one phone")).line(self.s("No forwards, no mailbox.")),
            "company_app" | "app" => {
                let app = self.c.apps.get(&rid).cloned();
                self.app(app)
            }
            // numbers.ref_id of a custom app is custom_apps.id, not apps.id
            "custom_app" => {
                let app = self.c.custom_apps.get(&rid).map(|(n, e)| (n.clone(), "custom".to_string(), *e));
                self.app(app)
            }
            _ => return None,
        };
        start.edge("", false, next);
        Some(start)
    }
}


pub fn did_flow(c: &Config, t: &Tr, did: i32) -> Option<Node> {
    let d = c.dids.get(&did)?.clone();
    Some(Walk { c, t, path: vec![], nodes: 0 }.did(&d))
}

pub fn internal_flow(c: &Config, t: &Tr, number: i16) -> Option<Node> {
    Walk { c, t, path: vec![], nodes: 0 }.internal(number)
}

/// A voice menu on its own (it may have no internal number, numbers and other menus lead to it).
pub fn ivr_flow(c: &Config, t: &Tr, id: i32) -> Option<Node> {
    let name = c.ivrs.get(&id)?.name.clone();
    let mut w = Walk { c, t, path: vec![], nodes: 0 };
    let mut start = Node::new("start", tf(t, "Call into the voice menu “{}”", &[&name]));
    let next = w.ivr(id);
    start.edge("", false, next);
    Some(start)
}

// --- Page -----------------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin_flow.html")]
struct FlowPage {
    c: Ctx,
    title: String,
    back: String,
    flow: String,
    recall_s: i32,
}

fn page(app: &AppState, a: &crate::auth::CurrentUser, section: &'static str, title: String, back: String, node: Node, recall_s: i32) -> Result<Response, AppError> {
    let c = Ctx::new(app, a).section(section);
    let flow = html(&node, &c.t);
    let p = FlowPage { c, title, back, flow, recall_s };
    render(p)
}

/// GET /admin/flow/did/{id}
pub async fn did_page(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let cfg = Config::load(&app.db).await?;
    let t = Ctx::new(&app, &a).t;
    let node = did_flow(&cfg, &t, id).ok_or(AppError::NotFound)?;
    let d = &cfg.dids[&id];
    let title = if d.label.is_empty() { d.e164.clone() } else { format!("{} ({})", d.label, d.e164) };
    page(&app, &a, "numbers", title, format!("/admin/numbers/{id}"), node, cfg.recall_s)
}

/// GET /admin/flow/ivr/{id}
pub async fn ivr_page(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let cfg = Config::load(&app.db).await?;
    let t = Ctx::new(&app, &a).t;
    let node = ivr_flow(&cfg, &t, id).ok_or(AppError::NotFound)?;
    let title = cfg.ivrs[&id].name.clone();
    page(&app, &a, "ivrs", title, format!("/admin/ivrs/{id}"), node, cfg.recall_s)
}

/// GET /admin/flow/{number}
pub async fn internal_page(State(app): State<AppState>, Admin(a): Admin, Path(number): Path<i16>) -> Result<Response, AppError> {
    let cfg = Config::load(&app.db).await?;
    let t = Ctx::new(&app, &a).t;
    let node = internal_flow(&cfg, &t, number).ok_or(AppError::NotFound)?;
    let (kind, rid) = cfg.numbers[&number].clone();
    let (section, back) = match kind.as_str() {
        "group" => ("groups", format!("/admin/groups/{rid}")),
        "ivr" => ("ivrs", format!("/admin/ivrs/{rid}")),
        "person" => ("users", format!("/admin/users/{rid}")),
        _ => ("numbers", "/admin/numbers".to_string()),
    };
    page(&app, &a, section, number.to_string(), back, node, cfg.recall_s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    fn cfg() -> Config {
        let mut c = Config::default();
        let user = |id, ext: i16, name: &str| UserRow {
            id,
            extension: ext,
            display_name: name.into(),
            active: true,
            ring_timeout_s: 20,
            ..Default::default()
        };
        c.users.insert(1, user(1, 1018, "Tom"));
        c.users.insert(2, user(2, 1010, "Grace"));
        c.users.insert(3, user(3, 1011, "Karen"));
        c.devices.push(DeviceRow { user_id: 1, label: "Handy".into(), sip_username: "a".into(), ring: true, ring_delay_s: 0 });
        c.devices.push(DeviceRow { user_id: 2, label: "Tisch".into(), sip_username: "b".into(), ring: true, ring_delay_s: 0 });
        c.devices.push(DeviceRow { user_id: 3, label: "Tisch".into(), sip_username: "c".into(), ring: true, ring_delay_s: 0 });
        let grp = |id, number: i16, name: &str, na: Option<i32>| GroupRow {
            id,
            number,
            name: name.into(),
            strategy: "ringall".into(),
            member_timeout_s: 20,
            total_timeout_s: 20,
            no_answer_target: na,
            ..Default::default()
        };
        c.groups.insert(11, grp(11, 5030, "Zentrale", Some(102)));
        c.groups.insert(10, grp(10, 5020, "Vertretung", Some(103)));
        c.members.push(MemberRow { group_id: 11, user_id: 1, enabled: true, logged_in: true });
        c.members.push(MemberRow { group_id: 10, user_id: 2, enabled: true, logged_in: true });
        c.members.push(MemberRow { group_id: 10, user_id: 3, enabled: true, logged_in: false });
        let tg = |id, kind: &str| TargetRow { id, kind: kind.into(), ..Default::default() };
        c.targets.insert(101, TargetRow { group_id: Some(11), ..tg(101, "group") });
        c.targets.insert(102, TargetRow { group_id: Some(10), ..tg(102, "group") });
        c.targets.insert(103, TargetRow { user_id: Some(1), ..tg(103, "mailbox_user") });
        c.dids.insert(2, DidRow { id: 2, e164: "+494012345053".into(), label: "Zentrale".into(), target_open: Some(101), ..Default::default() });
        c.registered = Some(["a", "b"].into_iter().map(String::from).collect());
        c
    }

    fn text(n: &Node) -> String {
        let mut s = format!("[{}] {}\n", n.class, n.title);
        for l in &n.lines {
            s += &format!("  {l}\n");
        }
        for (_, l) in &n.live {
            s += &format!("  ! {l}\n");
        }
        for e in &n.edges {
            s += &format!("-> {}{}\n", e.label, if e.active { " (now)" } else { "" });
            s += &text(&e.node);
        }
        s
    }

    #[test]
    fn chain_group_to_group_to_mailbox() {
        let c = cfg();
        let t = Tr(Lang::En);
        let s = text(&did_flow(&c, &t, 2).unwrap());
        assert!(s.contains("[group] Group Zentrale (5030)"), "{s}");
        assert!(s.contains("[group] Group Vertretung (5020)"), "{s}");
        assert!(s.contains("Karen (1011) – skipped right now (logged out)"), "{s}");
        assert!(s.contains("[mailbox] Mailbox of Tom (1018)"), "{s}");
    }

    #[test]
    fn forward_loop_is_cut() {
        let mut c = cfg();
        c.targets.insert(201, TargetRow { id: 201, kind: "user".into(), user_id: Some(2), ..Default::default() });
        c.targets.insert(202, TargetRow { id: 202, kind: "user".into(), user_id: Some(1), ..Default::default() });
        c.users.get_mut(&1).unwrap().forward_immediate = Some(201);
        c.users.get_mut(&2).unwrap().forward_immediate = Some(202);
        c.numbers.insert(1018, ("person".into(), 1));
        let t = Tr(Lang::En);
        let s = text(&internal_flow(&c, &t, 1018).unwrap());
        assert!(s.contains("back to Tom (1018) (loop)"), "{s}");
    }

    #[test]
    fn time_profile_branches() {
        let mut c = cfg();
        c.profiles.insert(7, ("Büro".into(), "closed".into()));
        let d = c.dids.get_mut(&2).unwrap();
        d.time_profile_id = Some(7);
        d.target_closed = Some(103);
        let t = Tr(Lang::En);
        let s = text(&did_flow(&c, &t, 2).unwrap());
        assert!(s.contains("-> closed (now)"), "{s}");
        assert!(s.contains("-> holiday or closing day\n[ref] same as “closed”"), "{s}");
    }

    #[test]
    fn dnd_and_unconnected() {
        let mut c = cfg();
        c.users.get_mut(&1).unwrap().dnd = true;
        c.numbers.insert(5030, ("group".into(), 11));
        let t = Tr(Lang::En);
        let s = text(&internal_flow(&c, &t, 5030).unwrap());
        assert!(s.contains("Tom (1018) – skipped right now (do not disturb)"), "{s}");
        assert!(s.contains("! Nobody would ring right now"), "{s}");
    }

    #[test]
    fn app_targets_by_the_right_id() {
        let mut c = cfg();
        c.apps.insert(5, ("Wartemusik".into(), "music".into(), false));
        c.custom_apps.insert(5, ("Türklingel".into(), true));
        c.numbers.insert(9100, ("custom_app".into(), 5));
        c.numbers.insert(8100, ("company_app".into(), 5));
        c.targets.insert(301, TargetRow { id: 301, kind: "app".into(), app_id: Some(5), ..Default::default() });
        c.dids.get_mut(&2).unwrap().target_open = Some(301);
        let t = Tr(Lang::En);
        let s = text(&internal_flow(&c, &t, 9100).unwrap());
        assert!(s.contains("[app] App “Türklingel”\n  Own dialplan block"), "{s}");
        let s = text(&internal_flow(&c, &t, 8100).unwrap());
        assert!(s.contains("[app] App “Wartemusik”\n  Not active"), "{s}");
        let s = text(&did_flow(&c, &t, 2).unwrap());
        assert!(s.contains("[app] App “Wartemusik”\n  Not active"), "{s}");
    }

    #[test]
    fn outbound_number_of_a_person() {
        let mut c = cfg();
        let u = c.users.get_mut(&1).unwrap();
        u.out_e164 = Some("+494012345054".into());
        u.out_trunk = Some("054".into());
        u.out_never_shown = true;
        c.numbers.insert(1018, ("person".into(), 1));
        let t = Tr(Lang::En);
        let s = text(&internal_flow(&c, &t, 1018).unwrap());
        assert!(s.contains("Own calls out: preferably over 054 with the number +494012345054"), "{s}");
        assert!(s.contains("! Outbound number +494012345054 belongs to trunk 054"), "{s}");
        let s = text(&internal_flow(&c, &Tr(Lang::De), 1018).unwrap());
        assert!(s.contains("über 054 mit der Nummer +494012345054"), "{s}");
    }

    #[test]
    fn html_escapes() {
        let n = Node::new("start", "<b>&");
        assert_eq!(html(&n, &Tr(Lang::En)), "<div class=\"fn fn-start\"><div class=\"fn-title\">&lt;b&gt;&amp;</div></div>");
    }
}
