// SPDX-License-Identifier: GPL-3.0-or-later
//! Webhooks: HTTP requests to other systems (CRM, ERP, chat, automation) on events, built from
//! templates. Patchbay knows no target system; everything specific lives in the hook files.
//!
//! Hooks are static files: `/config/webhooks.d/*.toml` (PBX_WEBHOOKS_DIR), read at startup, each
//! with one or more `[[webhook]]` tables:
//!
//! ```toml
//! [[webhook]]
//! name = "crm-calllog"                  # unique
//! events = ["call.ended"]               # call.ended, note.created, note.updated, note.deleted, voicemail.new
//! when = "call.external and contact"    # optional expression on the template data
//! method = "POST"                       # default POST
//! url = "https://crm.example/api/calls?id={{ contact.x.some_id }}"
//! headers = { Authorization = "Bearer {{ secret('crm_token') }}", "Content-Type" = "application/json" }
//! body = '''{ "phone": {{ remote.e164 | tojson }} }'''
//! private_notes = false                 # also notes only their author sees (default: shared ones only)
//! timeout_s = 15
//! max_attempts = 12                     # retries with growing pauses (30 s … 6 h)
//! enabled = true
//! ```
//!
//! Templates: minijinja (Jinja syntax). `secret('name')` reads `/run/patchbay-web/<name>` (the
//! secrets patchbay-web gets, PBX_DIR/secrets/patchbay-web/). Missing values render empty, so
//! optional parts work with `{% if contact %}`. The data of an event is stored with the delivery
//! (`webhook_deliveries.payload`); the request is rendered when it is sent, with the hook as it is then.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use minijinja::{Environment, UndefinedBehavior, Value};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

pub const EVENTS: [&str; 5] = ["call.ended", "note.created", "note.updated", "note.deleted", "voicemail.new"];
const SECRETS_DIR: &str = "/run/patchbay-web";

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub name: String,
    pub events: Vec<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub private_notes: bool,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    #[serde(default = "default_attempts")]
    pub max_attempts: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// file the hook comes from (set when loading)
    #[serde(skip)]
    pub file: String,
}

fn default_method() -> String {
    "POST".into()
}
fn default_timeout() -> u64 {
    15
}
fn default_attempts() -> i32 {
    12
}
fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    #[serde(default)]
    webhook: Vec<Hook>,
}

/// Loaded hooks and the problems found while loading (shown on the admin page)
#[derive(Default)]
pub struct Hooks {
    pub hooks: Vec<Hook>,
    pub errors: Vec<String>,
}

impl Hooks {
    pub fn get(&self, name: &str) -> Option<&Hook> {
        self.hooks.iter().find(|h| h.name == name)
    }
}

fn env<'a>(dry_run: bool) -> Environment<'a> {
    let mut env = Environment::new();
    // missing values (no contact, no DID, …) render empty instead of failing the request
    env.set_undefined_behavior(UndefinedBehavior::Chainable);
    env.add_function("secret", move |name: String| -> Result<String, minijinja::Error> {
        if dry_run {
            return Ok("***".into());
        }
        let ok = !name.is_empty() && name.len() <= 100 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') && !name.starts_with('.');
        if !ok {
            return Err(minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, format!("secret('{name}'): letters, digits, . _ - only")));
        }
        std::fs::read_to_string(format!("{SECRETS_DIR}/{name}"))
            .map(|s| s.trim().to_string())
            .map_err(|_| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, format!("secret('{name}'): {SECRETS_DIR}/{name} not readable")))
    });
    env
}

/// Checks a hook's templates and expression (syntax only).
fn check(h: &Hook) -> Result<()> {
    if h.name.is_empty() || !h.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("name {:?}: letters, digits, - _ . only", h.name);
    }
    for e in &h.events {
        if !EVENTS.contains(&e.as_str()) {
            bail!("{}: unknown event {e:?} (known: {})", h.name, EVENTS.join(", "));
        }
    }
    if h.events.is_empty() {
        bail!("{}: no events", h.name);
    }
    if !matches!(h.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
        bail!("{}: method {:?}", h.name, h.method);
    }
    let env = env(true);
    if let Some(w) = &h.when {
        env.compile_expression(w).with_context(|| format!("{}: when", h.name))?;
    }
    env.template_from_str(&h.url).with_context(|| format!("{}: url", h.name))?;
    for (k, v) in &h.headers {
        env.template_from_str(v).with_context(|| format!("{}: header {k}", h.name))?;
    }
    if let Some(b) = &h.body {
        env.template_from_str(b).with_context(|| format!("{}: body", h.name))?;
    }
    Ok(())
}

/// Reads all `*.toml` files of the directory (sorted by name). Broken files or hooks are skipped
/// and reported; the others work.
pub fn load(dir: &Path) -> Hooks {
    let mut out = Hooks::default();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    files.sort();
    for f in files {
        let fname = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = std::fs::read_to_string(&f).map_err(anyhow::Error::from).and_then(|s| toml::from_str::<HookFile>(&s).map_err(anyhow::Error::from));
        match parsed {
            Err(e) => out.errors.push(format!("{fname}: {e:#}")),
            Ok(hf) => {
                for mut h in hf.webhook {
                    h.file = fname.clone();
                    if let Err(e) = check(&h) {
                        out.errors.push(format!("{fname}: {e:#}"));
                    } else if out.get(&h.name).is_some() {
                        out.errors.push(format!("{fname}: hook name {:?} used twice", h.name));
                    } else {
                        out.hooks.push(h);
                    }
                }
            }
        }
    }
    for e in &out.errors {
        tracing::warn!("webhooks: {e}");
    }
    if !out.hooks.is_empty() {
        tracing::info!("webhooks: {} hooks from {}", out.hooks.len(), dir.display());
    }
    out
}

pub fn dir() -> String {
    std::env::var("PBX_WEBHOOKS_DIR").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "/config/webhooks.d".into())
}

/// Does the hook want this payload (event, `when`)? Notes only author-visible are skipped unless
/// the hook asks for them.
fn wants(h: &Hook, event: &str, payload: &serde_json::Value) -> Result<bool> {
    if !h.enabled || !h.events.iter().any(|e| e == event) {
        return Ok(false);
    }
    if event.starts_with("note.") && !h.private_notes && payload.pointer("/note/shared") != Some(&json!(true)) {
        return Ok(false);
    }
    match &h.when {
        None => Ok(true),
        Some(w) => {
            let env = env(true);
            let expr = env.compile_expression(w)?;
            Ok(expr.eval(Value::from_serialize(payload))?.is_true())
        }
    }
}

/// Queues an event for every hook that wants it.
pub async fn enqueue(app: &AppState, event: &str, payload: serde_json::Value) {
    for h in &app.webhooks.hooks {
        // notes of a call: only shared ones unless the hook asks for private ones
        let mut p = payload.clone();
        if !h.private_notes {
            if let Some(serde_json::Value::Array(notes)) = p.get_mut("notes") {
                notes.retain(|n| n.get("shared") == Some(&json!(true)));
            }
        }
        match wants(h, event, &p) {
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!("webhook {}: when: {e:#}", h.name);
                continue;
            }
            Ok(true) => {}
        }
        let r = sqlx::query("INSERT INTO webhook_deliveries (hook, event, payload) VALUES ($1, $2, $3)").bind(&h.name).bind(event).bind(&p).execute(&app.db).await;
        if let Err(e) = r {
            tracing::warn!("webhook {}: queueing failed: {e:#}", h.name);
        }
    }
}

/// A request rendered from a hook and a payload
#[derive(Debug, Clone)]
pub struct Rendered {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

pub fn render(h: &Hook, payload: &serde_json::Value, dry_run: bool) -> Result<Rendered> {
    let env = env(dry_run);
    let ctx = Value::from_serialize(payload);
    let r = |name: &str, t: &str| -> Result<String> { env.render_str(t, &ctx).with_context(|| format!("{}: {name}", h.name)) };
    let url = r("url", &h.url)?.trim().to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("{}: url renders to {:?}, not an http(s) URL", h.name, url.chars().take(80).collect::<String>());
    }
    let mut headers = vec![];
    for (k, v) in &h.headers {
        let v = r(&format!("header {k}"), v)?;
        if v.contains('\r') || v.contains('\n') {
            bail!("{}: header {k} contains a line break", h.name);
        }
        headers.push((k.clone(), v));
    }
    let body = match &h.body {
        Some(b) => Some(r("body", b)?),
        None => None,
    };
    Ok(Rendered { method: h.method.clone(), url, headers, body })
}

async fn send(http: &reqwest::Client, h: &Hook, req: &Rendered) -> Result<(u16, String)> {
    let mut b = http.request(reqwest::Method::from_bytes(req.method.as_bytes())?, &req.url).timeout(Duration::from_secs(h.timeout_s.clamp(1, 120)));
    for (k, v) in &req.headers {
        b = b.header(k, v);
    }
    if let Some(body) = &req.body {
        b = b.body(body.clone());
    }
    let r = b.send().await.map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
    let status = r.status().as_u16();
    let text: String = r.text().await.unwrap_or_default().chars().take(500).collect();
    Ok((status, text))
}

/// Pause before attempt n+1: 30 s, 1 min, 2 min, … at most 6 h
fn backoff(attempts: i32) -> i64 {
    (30i64 << attempts.clamp(0, 10)).min(6 * 3600)
}

/// Delivery loop: every few seconds the due deliveries are rendered and sent.
pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        // deliveries interrupted by a restart are retried
        let _ = sqlx::query("UPDATE webhook_deliveries SET status = 'pending' WHERE status = 'sending'").execute(&app.db).await;
        let http = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("patchbay/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("webhooks: {e:#}");
                return;
            }
        };
        loop {
            if let Err(e) = deliver_due(&app, &http).await {
                tracing::warn!("webhooks: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn deliver_due(app: &AppState, http: &reqwest::Client) -> Result<()> {
    let due: Vec<(i64, String, serde_json::Value, i32)> = sqlx::query_as(
        "UPDATE webhook_deliveries SET status = 'sending', attempts = attempts + 1
         WHERE id IN (SELECT id FROM webhook_deliveries WHERE status = 'pending' AND next_at <= now() ORDER BY id LIMIT 20 FOR UPDATE SKIP LOCKED)
         RETURNING id, hook, payload, attempts",
    )
    .fetch_all(&app.db)
    .await?;
    for (id, hook, payload, attempts) in due {
        let Some(h) = app.webhooks.get(&hook) else {
            finish(app, id, "failed", None, "hook no longer configured", None).await?;
            continue;
        };
        let req = match render(h, &payload, false) {
            Ok(r) => r,
            Err(e) => {
                // a template error does not go away by retrying
                finish(app, id, "failed", None, &format!("{e:#}"), None).await?;
                continue;
            }
        };
        match send(http, h, &req).await {
            Ok((code, _)) if (200..300).contains(&code) => finish(app, id, "sent", Some(code), "", None).await?,
            Ok((code, text)) => {
                let retry = attempts < h.max_attempts && (code == 429 || code >= 500 || code == 408);
                let next = retry.then(|| backoff(attempts));
                finish(app, id, if retry { "pending" } else { "failed" }, Some(code), &text, next).await?;
            }
            Err(e) => {
                let retry = attempts < h.max_attempts;
                let next = retry.then(|| backoff(attempts));
                finish(app, id, if retry { "pending" } else { "failed" }, None, &format!("{e:#}"), next).await?;
            }
        }
    }
    Ok(())
}

async fn finish(app: &AppState, id: i64, status: &str, code: Option<u16>, error: &str, next_in_s: Option<i64>) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET status = $2, last_status = $3, last_error = NULLIF($4, ''),
                sent_at = CASE WHEN $2 = 'sent' THEN now() ELSE sent_at END,
                next_at = now() + make_interval(secs => COALESCE($5, 0)::double precision)
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(code.map(|c| c as i32))
    .bind(error.chars().take(500).collect::<String>())
    .bind(next_in_s)
    .execute(&app.db)
    .await?;
    if status == "failed" {
        tracing::warn!("webhook delivery {id} failed: {}", error.chars().take(200).collect::<String>());
    }
    Ok(())
}

// --- Template data -----------------------------------------------------------------------------------

/// A person for templates: extension, name, e-mail
async fn person(app: &AppState, uid: i32) -> Option<serde_json::Value> {
    let r: Option<(i16, String, Option<String>)> =
        sqlx::query_as("SELECT extension, display_name, email FROM users WHERE id = $1").bind(uid).fetch_optional(&app.db).await.ok()?;
    r.map(|(ext, name, email)| json!({"ext": ext.to_string(), "name": name, "email": email}))
}

/// The contact for a number: the person's own contacts, the shared phone book, then the external
/// phone books (with their X- fields as `x`).
pub async fn contact_for(app: &AppState, uid: Option<i32>, e164: &str) -> Option<serde_json::Value> {
    let own: Option<(String, Option<String>, Option<String>, bool, i32)> = sqlx::query_as(
        "SELECT name, organization, email, user_id IS NULL, id FROM contacts
         WHERE (user_id = $1 OR ($2 AND user_id IS NULL AND deleted_at IS NULL)) AND (phone_e164 = $3 OR mobile_e164 = $3)
         ORDER BY user_id IS NULL, id LIMIT 1",
    )
    .bind(uid.unwrap_or(0))
    .bind(app.cfg.shared_contacts.on())
    .bind(e164)
    .fetch_optional(&app.db)
    .await
    .ok()?;
    if let Some((name, org, email, shared, id)) = own {
        return Some(json!({"source": if shared { "shared" } else { "personal" }, "id": id, "name": name, "organization": org, "email": email, "x": {}}));
    }
    let ext: Option<(String, String, Option<String>, Option<String>, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT book, name, organization, email, url, extra FROM external_contacts WHERE phone_e164 = $1 OR mobile_e164 = $1 ORDER BY book, res LIMIT 1",
    )
    .bind(e164)
    .fetch_optional(&app.db)
    .await
    .ok()?;
    ext.map(|(book, name, org, email, url, extra)| {
        let book_name = app.cfg.external_books.iter().find(|b| b.id == book).map(|b| b.name.clone()).unwrap_or_else(|| book.clone());
        json!({"source": "external", "book": book, "book_name": book_name, "name": name, "organization": org, "email": email, "url": url, "x": extra})
    })
}

fn site(app: &AppState) -> serde_json::Value {
    json!({"title": app.cfg.site_title, "url": app.cfg.public_url})
}

/// Times for templates: UTC (ISO 8601) and local (site time zone)
async fn times(app: &AppState, epoch: f64) -> (String, String) {
    let utc = chrono::DateTime::from_timestamp(epoch as i64, 0).map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
    let local: String = sqlx::query_scalar("SELECT to_char(to_timestamp($1) AT TIME ZONE $2, 'YYYY-MM-DD HH24:MI:SS')")
        .bind(epoch)
        .bind(&app.cfg.timezone)
        .fetch_one(&app.db)
        .await
        .unwrap_or_default();
    (utc, local)
}

/// call.ended: from the recorded call flow (calltrack.rs), after it was stored.
pub async fn on_call(app: &AppState, f: &crate::calltrack::Flow) {
    if app.webhooks.hooks.is_empty() {
        return;
    }
    let payload = match call_payload(app, f).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("webhooks: call {}: {e:#}", f.linkedid);
            return;
        }
    };
    enqueue(app, "call.ended", payload).await;
}

async fn call_payload(app: &AppState, f: &crate::calltrack::Flow) -> Result<serde_json::Value> {
    // endpoint → (user id, device label, device number); trunk endpoints → name
    let dev = |ep: &str| {
        let ep = ep.to_string();
        async move {
            sqlx::query_as::<_, (i32, String, Option<i16>)>("SELECT user_id, label, number FROM devices WHERE sip_username = $1")
                .bind(ep)
                .fetch_optional(&app.db)
                .await
                .ok()
                .flatten()
        }
    };
    let trunks: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("SELECT asterisk_ref, name FROM trunks").fetch_all(&app.db).await?.into_iter().collect();
    let trunk_of = |ep: &str| trunks.get(ep.strip_prefix("IAX2/").unwrap_or(ep)).cloned();
    let origin_dev = dev(&f.origin.ep).await;
    let trunk_leg = f.dials.iter().find(|d| d.is_trunk() && trunk_of(&d.ep).is_some());
    let from_trunk = trunk_of(&f.origin.ep).or_else(|| (!f.origin.chan.is_empty()).then(|| trunk_of(&f.origin.chan)).flatten());
    let (direction, remote_raw, trunk) = if let Some(t) = &from_trunk {
        ("in", f.origin.num.clone(), Some(t.clone()))
    } else if let Some(d) = trunk_leg {
        ("out", d.to.clone(), trunk_of(&d.ep))
    } else {
        ("internal", String::new(), None)
    };
    // the person: caller (outgoing), the one who answered (incoming), or the first person rung
    let answered_dev = match &f.answered_by {
        Some(ep) => dev(ep).await,
        None => None,
    };
    let uid: Option<i32> = match direction {
        "out" => origin_dev.as_ref().map(|d| d.0),
        _ => match &answered_dev {
            Some(d) => Some(d.0),
            None => match f.steps.iter().find(|s| s.kind == "user") {
                Some(s) => sqlx::query_scalar("SELECT id FROM users WHERE extension::text = $1 AND deleted_at IS NULL").bind(&s.number).fetch_optional(&app.db).await?,
                None => None,
            },
        },
    }
    .or(origin_dev.as_ref().map(|d| d.0));
    let user = match uid {
        Some(u) => person(app, u).await,
        None => None,
    };
    let device = if direction == "out" { origin_dev.as_ref() } else { answered_dev.as_ref() };
    let e164 = (!remote_raw.is_empty()).then(|| crate::blocklist::full_number(&remote_raw, &app.cfg.country_code)).flatten();
    let contact = match &e164 {
        Some(e) => contact_for(app, uid, e).await,
        None => None,
    };
    let ended = f.started + f.duration_s as f64;
    let (started_utc, started_local) = times(app, f.started).await;
    let (ended_utc, ended_local) = times(app, ended).await;
    // notes on the number written during the call or in the minute after it
    let notes: Vec<serde_json::Value> = match &e164 {
        Some(e) => sqlx::query_as::<_, (i32, String, bool, i32, String)>(
            "SELECT n.id, n.body, n.shared AND $4, n.user_id, u.display_name FROM call_notes n JOIN users u ON u.id = n.user_id
             WHERE n.e164 = $1 AND n.created_at BETWEEN to_timestamp($2) AND to_timestamp($3) + interval '1 minute' ORDER BY n.id",
        )
        .bind(e)
        .bind(f.started)
        .bind(ended)
        .bind(app.cfg.shared_notes)
        .fetch_all(&app.db)
        .await?
        .into_iter()
        .map(|(id, body, shared, author_id, author)| json!({"id": id, "body": body, "shared": shared, "author_id": author_id, "author": author}))
        .collect(),
        None => vec![],
    };
    let answered = f.talk_s.is_some();
    Ok(json!({
        "event": "call.ended",
        "site": site(app),
        "call": {
            "id": f.linkedid,
            "direction": direction,
            "incoming": direction == "in",
            "outgoing": direction == "out",
            "internal": direction == "internal",
            "external": direction != "internal",
            "answered": answered,
            "voicemail": f.voicemail,
            "started": started_utc,
            "ended": ended_utc,
            "started_local": started_local,
            "ended_local": ended_local,
            "started_epoch": f.started as i64,
            "ended_epoch": ended as i64,
            "duration_s": f.duration_s,
            "talk_s": f.talk_s.unwrap_or(0),
            "did": f.did,
            "trunk": trunk,
            "steps": f.steps,
        },
        "user": user,
        "device": device.map(|d| json!({"label": d.1, "number": d.2})),
        "remote": {"number": remote_raw, "e164": e164, "name": if direction == "in" { Some(f.origin.name.clone()).filter(|n| !n.is_empty()) } else { None }},
        "contact": contact,
        "notes": notes,
    }))
}

/// note.created / note.updated / note.deleted (call notes, notes.rs)
pub async fn on_note(app: &AppState, event: &str, note_id: i32, e164: &str, body: &str, shared: bool, author_id: i32) {
    if app.webhooks.hooks.is_empty() {
        return;
    }
    let author = person(app, author_id).await;
    let contact = contact_for(app, Some(author_id), e164).await;
    let (now_utc, now_local) = times(app, chrono::Utc::now().timestamp() as f64).await;
    let payload = json!({
        "event": event,
        "site": site(app),
        "note": {"id": note_id, "body": body, "shared": shared && app.cfg.shared_notes, "at": now_utc, "at_local": now_local, "at_epoch": chrono::Utc::now().timestamp()},
        "user": author,
        "remote": {"number": e164, "e164": e164},
        "contact": contact,
    });
    enqueue(app, event, payload).await;
}

/// voicemail.new from the event bus
pub fn spawn_bus(app: AppState) {
    if app.webhooks.hooks.iter().all(|h| !h.events.iter().any(|e| e == "voicemail.new")) {
        return;
    }
    let mut rx = app.events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let patchbay_notify::PbxEvent::VoicemailNew { id, caller, owner, duration_s, .. } = ev.as_ref() {
                        let e164 = crate::blocklist::full_number(&caller.number, &app.cfg.country_code);
                        let contact = match &e164 {
                            Some(e) => contact_for(&app, None, e).await,
                            None => None,
                        };
                        let (now_utc, now_local) = times(&app, chrono::Utc::now().timestamp() as f64).await;
                        let payload = json!({
                            "event": "voicemail.new",
                            "site": site(&app),
                            "voicemail": {"id": id, "duration_s": duration_s, "owner": serde_json::to_value(owner).unwrap_or_default(), "at": now_utc, "at_local": now_local},
                            "remote": {"number": caller.number, "e164": e164, "name": caller.name},
                            "contact": contact,
                        });
                        enqueue(&app, "voicemail.new", payload).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!("webhooks: {n} events missed"),
                Err(_) => break,
            }
        }
    });
}

/// Example data per event (admin page "test", when no real delivery exists yet)
pub fn sample(event: &str) -> serde_json::Value {
    let contact = json!({"source": "external", "book": "erp", "book_name": "ERP", "name": "Example Ltd", "organization": "Example Ltd",
                         "email": "info@example.org", "url": "https://erp.example.org/c/1", "x": {"example_id": "123"}});
    let user = json!({"ext": "1020", "name": "Jane Doe", "email": "jane@example.org"});
    let remote = json!({"number": "0401234567", "e164": "+49401234567", "name": "Example Ltd"});
    let site = json!({"title": "Phone system", "url": "https://pbx.example.org"});
    match event {
        e if e.starts_with("note.") => json!({"event": e, "site": site, "note": {"id": 1, "body": "Calls back on Monday", "shared": true,
            "at": "2026-01-02T09:00:00Z", "at_local": "2026-01-02 10:00:00", "at_epoch": 1767344400}, "user": user, "remote": remote, "contact": contact}),
        "voicemail.new" => json!({"event": "voicemail.new", "site": site, "voicemail": {"id": 1, "duration_s": 25, "owner": {"kind": "user", "number": "1020"},
            "at": "2026-01-02T09:00:00Z", "at_local": "2026-01-02 10:00:00"}, "remote": remote, "contact": contact}),
        _ => json!({"event": "call.ended", "site": site, "call": {"id": "1767344400.1", "direction": "in", "incoming": true, "outgoing": false,
            "internal": false, "external": true, "answered": true, "voicemail": false, "started": "2026-01-02T09:00:00Z", "ended": "2026-01-02T09:03:20Z",
            "started_local": "2026-01-02 10:00:00", "ended_local": "2026-01-02 10:03:20", "started_epoch": 1767344400, "ended_epoch": 1767344600, "duration_s": 200, "talk_s": 185, "did": "+49401111111",
            "trunk": "Main line", "steps": []}, "user": user, "device": {"label": "Desk phone", "number": 2004}, "remote": remote, "contact": contact,
            "notes": [{"id": 1, "body": "Wants a quote", "shared": true, "author_id": 1, "author": "Jane Doe"}]}),
    }
}

pub type Shared = Arc<Hooks>;

// --- Admin page ---------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
pub struct DeliveryRow {
    pub id: i64,
    pub hook: String,
    pub event: String,
    pub status: String,
    pub attempts: i32,
    pub created: String,
    pub next: String,
    pub last_status: Option<i32>,
    pub last_error: Option<String>,
}

#[derive(askama::Template)]
#[template(path = "admin_webhooks.html")]
struct WebhooksPage {
    c: crate::ui::Ctx,
    dir: String,
    hooks: Vec<Hook>,
    errors: Vec<String>,
    deliveries: Vec<DeliveryRow>,
    events: Vec<&'static str>,
    /// result of "test": (hook, event, method, url, headers, body or error, payload)
    test: Option<TestResult>,
}

pub struct TestResult {
    pub hook: String,
    pub event: String,
    pub source: &'static str,
    pub rendered: Result<Rendered, String>,
    pub wanted: Result<bool, String>,
    pub payload: String,
}

#[derive(Deserialize)]
pub struct TestForm {
    hook: String,
    event: String,
}

async fn page(app: &AppState, u: &crate::auth::CurrentUser, test: Option<TestResult>) -> Result<axum::response::Response, crate::AppError> {
    let deliveries: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT id, hook, event, status, attempts, to_char(created_at AT TIME ZONE $1, 'DD.MM.YY HH24:MI:SS') AS created,
                CASE WHEN status = 'pending' THEN to_char(next_at AT TIME ZONE $1, 'DD.MM. HH24:MI:SS') ELSE '' END AS next,
                last_status, last_error
         FROM webhook_deliveries ORDER BY id DESC LIMIT 200",
    )
    .bind(&app.cfg.timezone)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    crate::ui::render(WebhooksPage {
        c: crate::ui::Ctx::new(app, u).section("webhooks"),
        dir: dir(),
        hooks: app.webhooks.hooks.clone(),
        errors: app.webhooks.errors.clone(),
        deliveries,
        events: EVENTS.to_vec(),
        test,
    })
}

/// GET /admin/webhooks
pub async fn admin_page(axum::extract::State(app): axum::extract::State<AppState>, crate::auth::Admin(u): crate::auth::Admin) -> Result<axum::response::Response, crate::AppError> {
    page(&app, &u, None).await
}

/// POST /admin/webhooks/test: renders a hook with the data of the last delivery of the event (or an
/// example) without sending it; secrets are shown as ***
pub async fn admin_test(
    axum::extract::State(app): axum::extract::State<AppState>,
    crate::auth::Admin(u): crate::auth::Admin,
    axum::Form(f): axum::Form<TestForm>,
) -> Result<axum::response::Response, crate::AppError> {
    let h = app.webhooks.get(&f.hook).ok_or(crate::AppError::NotFound)?.clone();
    if !EVENTS.contains(&f.event.as_str()) {
        return Err(crate::AppError::BadRequest("Unknown event.".into()));
    }
    let last: Option<serde_json::Value> = sqlx::query_scalar("SELECT payload FROM webhook_deliveries WHERE event = $1 ORDER BY id DESC LIMIT 1")
        .bind(&f.event)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let (payload, source) = match last {
        Some(p) => (p, "last delivery"),
        None => (sample(&f.event), "example data"),
    };
    let test = TestResult {
        hook: h.name.clone(),
        event: f.event.clone(),
        source,
        rendered: render(&h, &payload, true).map_err(|e| format!("{e:#}")),
        wanted: wants(&h, &f.event, &payload).map_err(|e| format!("{e:#}")),
        payload: serde_json::to_string_pretty(&payload).unwrap_or_default(),
    };
    page(&app, &u, Some(test)).await
}

/// POST /admin/webhooks/{id}/retry: a failed delivery is sent again (counting from zero)
pub async fn admin_retry(
    axum::extract::State(app): axum::extract::State<AppState>,
    crate::auth::Admin(u): crate::auth::Admin,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<axum::response::Response, crate::AppError> {
    sqlx::query("UPDATE webhook_deliveries SET status = 'pending', attempts = 0, next_at = now() WHERE id = $1 AND status = 'failed'")
        .bind(id)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    crate::db::audit(&app.db, &u.session.sub, "webhook.retry", &format!("delivery:{id}"), json!({})).await?;
    Ok(axum::response::IntoResponse::into_response(axum::response::Redirect::to("/admin/webhooks")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(toml_src: &str) -> Hook {
        let mut f: HookFile = toml::from_str(toml_src).unwrap();
        let h = f.webhook.remove(0);
        check(&h).unwrap();
        h
    }

    #[test]
    fn render_call_hook() {
        let h = hook(
            r#"
[[webhook]]
name = "crm"
events = ["call.ended"]
when = "call.external and contact.x.example_id"
url = "https://crm.example/api/calls?person={{ contact.x.example_id }}"
headers = { "x-api-token" = "{{ secret('crm_token') }}", "Content-Type" = "application/json" }
body = '''{"subject": "Call {{ 'from' if call.incoming else 'to' }} {{ contact.name }}", "phone": {{ remote.e164 | tojson }}, "note": {{ notes | map(attribute='body') | join('\n') | tojson }}, "did": "{{ call.missing.deeper }}"}'''
"#,
        );
        let p = sample("call.ended");
        assert!(wants(&h, "call.ended", &p).unwrap());
        assert!(!wants(&h, "note.created", &p).unwrap());
        let r = render(&h, &p, true).unwrap();
        assert_eq!(r.url, "https://crm.example/api/calls?person=123");
        assert!(r.headers.contains(&("x-api-token".into(), "***".into())), "secrets masked in a dry run");
        let body: serde_json::Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["subject"], "Call from Example Ltd");
        assert_eq!(body["phone"], "+49401234567");
        assert_eq!(body["note"], "Wants a quote");
        assert_eq!(body["did"], "", "missing values render empty");
        // without a contact the condition is false
        let mut p2 = p.clone();
        p2["contact"] = serde_json::Value::Null;
        assert!(!wants(&h, "call.ended", &p2).unwrap());
    }

    #[test]
    fn checks_and_private_notes() {
        let bad = |s: &str| {
            let f: HookFile = toml::from_str(s).unwrap();
            check(&f.webhook[0]).is_err()
        };
        assert!(bad("[[webhook]]\nname = \"x\"\nevents = [\"call.started\"]\nurl = \"https://a\""));
        assert!(bad("[[webhook]]\nname = \"x\"\nevents = [\"call.ended\"]\nurl = \"https://a/{{ broken\""));
        assert!(bad("[[webhook]]\nname = \"x y\"\nevents = [\"call.ended\"]\nurl = \"https://a\""));
        assert!(toml::from_str::<HookFile>("[[webhook]]\nname = \"x\"\nevents = []\nurl = \"u\"\ntypo = 1").is_err(), "unknown keys refused");
        let h = hook("[[webhook]]\nname = \"n\"\nevents = [\"note.created\"]\nurl = \"https://a\"");
        let mut p = sample("note.created");
        assert!(wants(&h, "note.created", &p).unwrap());
        p["note"]["shared"] = json!(false);
        assert!(!wants(&h, "note.created", &p).unwrap(), "personal notes only with private_notes = true");
        // header injection and non-http URLs
        let h = hook("[[webhook]]\nname = \"h\"\nevents = [\"call.ended\"]\nurl = \"{{ contact.name }}\"");
        assert!(render(&h, &sample("call.ended"), true).is_err());
        let h = hook("[[webhook]]\nname = \"h\"\nevents = [\"call.ended\"]\nurl = \"https://a\"\nheaders = { X = \"{{ 'a\\nb' }}\" }");
        assert!(render(&h, &sample("call.ended"), true).is_err());
    }

    /// The examples in docs/webhooks.md parse, check and render with example data
    #[test]
    fn doc_examples() {
        let doc = include_str!("../../docs/webhooks.md");
        let mut n = 0;
        for block in doc.split("```toml\n").skip(1) {
            let src = block.split("```").next().unwrap();
            let f: HookFile = toml::from_str(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
            for h in f.webhook {
                check(&h).unwrap_or_else(|e| panic!("{e:#}"));
                for ev in &h.events {
                    let mut p = sample(ev);
                    p["contact"]["x"] = json!({"dolibarr_type": "contact", "dolibarr_id": "5", "dolibarr_socid": "1",
                        "dolibarr_extra_ext_crm_id": "296", "dolibarr_soc_extra_ext_crm_id": "413"});
                    let r = render(&h, &p, true).unwrap_or_else(|e| panic!("{e:#}"));
                    if r.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json") {
                        serde_json::from_str::<serde_json::Value>(r.body.as_deref().unwrap_or("{}"))
                            .unwrap_or_else(|e| panic!("{}: body is not JSON: {e}\n{}", h.name, r.body.unwrap_or_default()));
                    }
                    n += 1;
                }
            }
        }
        assert!(n >= 4);
    }

    #[test]
    fn retry_pauses() {
        assert_eq!(backoff(0), 30);
        assert_eq!(backoff(1), 60);
        assert_eq!(backoff(20), 6 * 3600);
    }

    #[test]
    fn load_dir() {
        let d = std::env::temp_dir().join(format!("pbx-webhooks-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.toml"), "[[webhook]]\nname = \"one\"\nevents = [\"call.ended\"]\nurl = \"https://a\"\n").unwrap();
        std::fs::write(d.join("b.toml"), "[[webhook]]\nname = \"one\"\nevents = [\"call.ended\"]\nurl = \"https://b\"\n").unwrap();
        std::fs::write(d.join("c.toml"), "not toml [").unwrap();
        std::fs::write(d.join("ignored.txt"), "x").unwrap();
        let hooks = load(&d);
        assert_eq!(hooks.hooks.len(), 1);
        assert_eq!(hooks.hooks[0].file, "a.toml");
        assert_eq!(hooks.errors.len(), 2, "duplicate name and broken file reported");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
