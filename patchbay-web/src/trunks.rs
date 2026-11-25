// SPDX-License-Identifier: GPL-3.0-or-later
//! Trunks (admin): IAX2 peers and SIP trunks managed in the web UI, their inbound rules and permissions.
//! IAX peers are written to /generated/iax_peers.conf, SIP trunks to /generated/pjsip_trunks.conf;
//! after a reload every object is checked and the previous file is restored if one is missing.
//! Trunks defined outside patchbay-web (managed = false: a row added by hand for an endpoint/peer in a
//! site's own static Asterisk configuration) are listed with permissions and channel limit only.

use std::time::Duration;

use anyhow::Context;

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form as MultiForm;
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use crate::{
    AppError, AppState,
    admin::{TargetInput, save_target},
    ami,
    auth::Admin,
    crypto, db,
    ui::{Ctx, TARGET_DESC, TARGET_JOINS, TargetOptions, TargetSel, render},
};

const OUT: &str = "/generated/iax_peers.conf";
const OUT_SIP: &str = "/generated/pjsip_trunks.conf";
/// Codecs a SIP trunk may offer (Opus: codec_opus_open_source, see asterisk/Dockerfile).
pub const CODECS: [&str; 4] = ["g722", "alaw", "ulaw", "opus"];
pub const KINDS: [&str; 5] = ["person", "group", "ivr", "device", "app"];


#[derive(FromRow, Clone)]
pub struct TrunkRow {
    id: i32,
    name: String,
    kind: String,
    asterisk_ref: String,
    enabled: bool,
    managed: bool,
    host: Option<String>,
    port: Option<i32>,
    has_secret: bool,
    encryption: bool,
    iax_trunk: bool,
    calltoken: String,
    site_prefix: Option<String>,
    /// IAX: our login at the peer (the peer's section name for us)
    iax_username: Option<String>,
    route_classes: Vec<String>,
    max_channels: Option<i32>,
    inbound_mode: String,
    sip_transport: String,
    username: Option<String>,
    from_user: Option<String>,
    from_domain: Option<String>,
    outbound_proxy: Option<String>,
    media_encryption: String,
    codecs: Vec<String>,
    identify_match: Option<String>,
    /// SIP: the provider shows only this trunk's own numbers (route_trunk_order() caller ID)
    clip_own_only: bool,
    /// incoming calls: take over the caller name / number the peer sends
    #[sqlx(default)]
    accept_caller_name: bool,
    #[sqlx(default)]
    accept_caller_number: bool,
}

fn o(s: &Option<String>) -> String {
    s.clone().unwrap_or_default()
}

impl TrunkRow {
    fn kind_label(&self) -> &'static str {
        match self.kind.as_str() {
            "iax2" => "IAX peer",
            "sip_ip" => "SIP (IP-based)",
            _ => "SIP (registration)",
        }
    }
    fn is_sip(&self) -> bool {
        self.kind.starts_with("sip")
    }
    fn has_codec(&self, c: &str) -> bool {
        self.codecs.iter().any(|x| x == c)
    }
    fn username_str(&self) -> String {
        o(&self.username)
    }
    fn from_user_str(&self) -> String {
        o(&self.from_user)
    }
    fn from_domain_str(&self) -> String {
        o(&self.from_domain)
    }
    fn proxy_str(&self) -> String {
        o(&self.outbound_proxy)
    }
    fn identify_str(&self) -> String {
        o(&self.identify_match)
    }
    fn has_class(&self, c: &str) -> bool {
        self.route_classes.iter().any(|x| x == c)
    }
    fn host_str(&self) -> String {
        self.host.clone().unwrap_or_default()
    }
    fn prefix_str(&self) -> String {
        self.site_prefix.clone().unwrap_or_default()
    }
    fn iax_user_str(&self) -> String {
        self.iax_username.clone().unwrap_or_default()
    }
    /// IAX peer without "our login at the peer": render_peers leaves it out, so it is not loaded
    fn login_missing(&self) -> bool {
        self.kind == "iax2" && self.managed && self.iax_username.is_none()
    }
    fn max_str(&self) -> String {
        self.max_channels.map(|m| m.to_string()).unwrap_or_default()
    }
}

const TRUNK_SELECT: &str = "SELECT id, name, kind::text AS kind, asterisk_ref, enabled, managed, host, port,
        secret_enc IS NOT NULL AS has_secret, encryption, iax_trunk, calltoken, site_prefix, iax_username, route_classes, max_channels,
        inbound_mode::text AS inbound_mode, sip_transport, username, from_user, from_domain, outbound_proxy, media_encryption,
        codecs, identify_match, clip_own_only, accept_caller_name, accept_caller_number
    FROM trunks WHERE deleted_at IS NULL";

// --- Config generation ----------------------------------------------------------------------

async fn render_peers(app: &AppState) -> anyhow::Result<(String, Vec<String>)> {
    let key = crypto::load_key()?;
    let rows: Vec<(String, String, Option<i32>, Option<String>, bool, bool, String, Option<String>)> = sqlx::query_as(
        "SELECT asterisk_ref, host, port, secret_enc, encryption, iax_trunk, calltoken, iax_username FROM trunks
         WHERE kind = 'iax2' AND managed AND enabled AND deleted_at IS NULL AND host IS NOT NULL ORDER BY id",
    )
    .fetch_all(&app.db)
    .await?;
    let mut out = String::from("; Generated by patchbay-web from the trunk table. Do not edit.\n");
    let mut names = vec![];
    for (name, host, port, secret, enc, trunk, calltoken, login) in rows {
        let Some(secret) = secret else { continue };
        // the trunk page flags it (TrunkRow::login_missing); without a login the peer cannot work
        let Some(login) = login else {
            tracing::warn!("IAX peer {name}: no login at the peer (iax_username) set, left out of the configuration");
            continue;
        };
        let secret = crypto::decrypt(&key, &secret)?;
        out += &format!(
            "\n[{name}]\ntype = friend\nhost = {host}\nport = {port}\n\
             ; our login at the peer (its section name for us)\nusername = {login}\n\
             secret = {secret}\nauth = md5\n\
             encryption = {enc}\nforceencryption = {force}\n\
             requirecalltoken = {calltoken}\ntrunk = {trunk}\ntransfer = no\nqualify = yes\n\
             context = from-iax\nsetvar = PBX_TRUNK={name}\n\
             disallow = all\nallow = g722\nallow = alaw\n\
             {acl}",
            port = port.unwrap_or(4569),
            enc = if enc { "aes128" } else { "no" },
            force = if enc { "yes" } else { "no" },
            trunk = if trunk { "yes" } else { "no" },
            // source ACL only for IP addresses (Asterisk ACLs do not resolve host names)
            acl = match host.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(_)) => format!("deny = 0.0.0.0/0.0.0.0\npermit = {host}/255.255.255.255\n"),
                Ok(std::net::IpAddr::V6(_)) => format!("deny = ::/0\npermit = {host}/128\n"),
                Err(_) => String::new(),
            },
        );
        names.push(name);
    }
    Ok((out, names))
}

/// One config change at a time (concurrent saves would restore each other's "previous" file).
static APPLY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Writes the peers file, reloads chan_iax2 and checks every peer; restores the previous file on failure.
pub async fn apply(app: &AppState) -> anyhow::Result<()> {
    let _guard = APPLY.lock().await;
    let (content, names) = render_peers(app).await?;
    let current = tokio::fs::read_to_string(OUT).await.unwrap_or_default();
    if current == content {
        return Ok(());
    }
    write_atomic(OUT, &content).await?;
    // the file contains secrets: $PBX_STATE_DIR/generated is 750 65534:999 (patchbay-init), so only
    // patchbay-web and the asterisk group can read it
    ami::command("iax2 reload").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // an AMI failure is not a missing peer: keep the new file and report
    let peers = ami::iax_peers().await.context("verification via AMI failed (config written)")?;
    for n in &names {
        if !peers.contains_key(n) {
            tokio::fs::write(OUT, &current).await?;
            ami::command("iax2 reload").await?;
            anyhow::bail!("peer {n} was not loaded by Asterisk; previous configuration restored");
        }
    }
    Ok(())
}

type SipRow = (
    String, String, Option<String>, Option<i32>, Option<String>, String, Option<String>, Option<String>,
    Option<String>, Option<String>, String, Vec<String>, Option<String>, i32,
);

async fn render_sip(app: &AppState) -> anyhow::Result<(String, Vec<(String, bool)>)> {
    let key = crypto::load_key()?;
    let rows: Vec<SipRow> = sqlx::query_as(
        "SELECT asterisk_ref, kind::text, host, port, secret_enc, sip_transport, username, from_user, from_domain,
                outbound_proxy, media_encryption, codecs, identify_match, reg_expiration
         FROM trunks WHERE kind IN ('sip_register', 'sip_ip') AND managed AND enabled AND deleted_at IS NULL
           AND host IS NOT NULL ORDER BY id",
    )
    .fetch_all(&app.db)
    .await?;
    let mut out = String::from("; Generated by patchbay-web from the trunk table. Do not edit.\n");
    let mut names = vec![];
    for (name, kind, host, port, secret, transport, user, from_user, from_domain, proxy, media, codecs, ident, expiration) in rows {
        let Some(host) = host else { continue };
        let register = kind == "sip_register";
        let hostport = match port {
            Some(p) if p > 0 => format!("{host}:{p}"),
            _ => host.clone(),
        };
        let proxy_line = proxy.as_deref().filter(|p| !p.is_empty()).map(|p| format!("outbound_proxy = sip:{p}\\;lr\n")).unwrap_or_default();
        let user = user.filter(|u| !u.is_empty());
        let mut auth = String::new();
        if let (Some(u), Some(s)) = (&user, &secret) {
            let pw = crypto::decrypt(&key, s)?;
            out += &format!("\n[{name}-auth]\ntype = auth\nauth_type = digest\nusername = {u}\npassword = {pw}\n");
            auth = format!("outbound_auth = {name}-auth\n");
        }
        let from_user = from_user.filter(|u| !u.is_empty()).or(user.clone());
        let from_domain = from_domain.filter(|d| !d.is_empty()).unwrap_or_else(|| host.clone());
        out += &format!(
            "\n[{name}]\ntype = aor\ncontact = sip:{hostport}\nqualify_frequency = 60\n{proxy_line}"
        );
        out += &format!(
            "\n[{name}]\ntype = endpoint\ntransport = transport-{transport}\ncontext = from-sip-trunk\n\
             disallow = all\nallow = {codecs}\ndtmf_mode = rfc4733\nrtp_symmetric = yes\nforce_rport = yes\n\
             rewrite_contact = yes\ndirect_media = no\nsend_pai = no\ntrust_id_inbound = yes\n\
             media_encryption = {media}\naors = {name}\n{auth}{proxy_line}{from_user}from_domain = {from_domain}\n\
             set_var = PBX_TRUNK={name}\n",
            codecs = codecs.join(","),
            from_user = from_user.as_deref().map(|u| format!("from_user = {u}\ncontact_user = {u}\n")).unwrap_or_default(),
        );
        if register && let Some(u) = &user {
            // never give up: Asterisk's default stops after 10 failed attempts (a WAN outage of
            // ~10 minutes, e.g. a nightly reconnect, left the trunk unregistered until a restart),
            // and fatal answers (401/407 with auth_rejection_permanent = yes, the default, 404 …)
            // ended it for good; those and 403 are now retried every 10 minutes (no hammering
            // with bad credentials), timeouts and temporary errors every minute
            out += &format!(
                "\n[{name}]\ntype = registration\ntransport = transport-{transport}\n{auth}\
                 server_uri = sip:{hostport}\nclient_uri = sip:{u}@{from_domain}\ncontact_user = {u}\n{proxy_line}\
                 retry_interval = 60\nmax_retries = 1000000\nforbidden_retry_interval = 600\nfatal_retry_interval = 600\n\
                 expiration = {expiration}\nline = yes\nendpoint = {name}\n"
            );
        }
        let mut matches = vec![host.clone()];
        if let Some(p) = proxy.as_deref().filter(|p| !p.is_empty()) {
            matches.push(p.split(':').next().unwrap_or(p).to_string());
        }
        matches.extend(ident.unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        out += &format!("\n[{name}-identify]\ntype = identify\nendpoint = {name}\nmatch = {}\n", matches.join(","));
        names.push((name, register && user.is_some()));
    }
    Ok((out, names))
}

/// Writes the SIP trunk file, reloads res_pjsip and checks every endpoint; restores the previous file on failure.
pub async fn apply_sip(app: &AppState) -> anyhow::Result<()> {
    let _guard = APPLY.lock().await;
    let (content, names) = render_sip(app).await?;
    let current = tokio::fs::read_to_string(OUT_SIP).await.unwrap_or_default();
    if current == content {
        return Ok(());
    }
    write_atomic(OUT_SIP, &content).await?;
    // Reloads pjsip.conf (trunk endpoints, AORs, auths, identifies and registrations live on the
    // res_pjsip sorcery instance). Transports are not touched (allow_reload = no) and the device
    // caches survive (sorcery.conf: expire_on_reload = no), so registered devices are not affected.
    ami::command("module reload res_pjsip.so").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let endpoints = ami::pjsip_endpoints().await.context("verification via AMI failed (config written)")?;
    let regs = if names.iter().any(|(_, r)| *r) {
        ami::outbound_registrations().await.context("verification via AMI failed (config written)")?
    } else {
        Default::default()
    };
    for (n, register) in &names {
        if !endpoints.contains(n) || (*register && !regs.contains_key(n)) {
            tokio::fs::write(OUT_SIP, &current).await?;
            ami::command("module reload res_pjsip.so").await?;
            anyhow::bail!("SIP trunk {n} was not loaded by Asterisk; previous configuration restored");
        }
    }
    Ok(())
}

async fn apply_all(app: &AppState) -> anyhow::Result<()> {
    let a = apply(app).await;
    let b = apply_sip(app).await;
    a.and(b)
}

/// Status per IAX peer from Asterisk ("OK (12 ms)", "UNREACHABLE", …)
async fn iax_status() -> std::collections::HashMap<String, String> {
    let mut m = ami::iax_peers().await.unwrap_or_default();
    // SIP trunks without registration: reachability of the AOR contact (qualify)
    if let Ok(out) = ami::command("pjsip show contacts").await {
        for l in out.lines().map(str::trim).filter(|l| l.starts_with("Contact:")) {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if let (Some(aor), Some(st)) = (parts.get(1).and_then(|c| c.split('/').next()), parts.get(3)) {
                let s = match *st {
                    "Avail" => format!("OK ({} ms)", parts.get(4).and_then(|r| r.parse::<f64>().ok()).map(|r| r.round() as i64).unwrap_or(0)),
                    "Unavail" => "UNREACHABLE".into(),
                    other => other.to_string(),
                };
                m.insert(aor.to_string(), s);
            }
        }
    }
    if let Ok(list) = ami::list("PJSIPShowRegistrationsOutbound", "OutboundRegistrationDetail").await {
        for e in list {
            if let (Some(n), Some(s)) = (e.get("Endpoint").or(e.get("ObjectName")), e.get("Status")) {
                m.insert(n.clone(), s.clone());
            }
        }
    }
    m
}

// --- Pages --------------------------------------------------------------------------------

pub struct TrunkView {
    t: TrunkRow,
    status: String,
}

#[derive(Template)]
#[template(path = "admin_trunks.html")]
struct TrunksPage {
    c: Ctx,
    trunks: Vec<TrunkView>,
}

pub async fn trunks(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let rows: Vec<TrunkRow> = sqlx::query_as(&format!("{TRUNK_SELECT} ORDER BY kind, name")).fetch_all(&app.db).await.map_err(anyhow::Error::from)?;
    let st = iax_status().await;
    let trunks = rows
        .into_iter()
        .map(|t| {
            let status = st.get(&t.asterisk_ref).cloned().unwrap_or_else(|| "–".into());
            TrunkView { t, status }
        })
        .collect();
    render(TrunksPage { c: Ctx::new(&app, &a).section("trunks"), trunks })
}

#[derive(FromRow)]
pub struct RuleRow {
    id: i32,
    priority: i16,
    match_prefix: String,
    min_len: i16,
    max_len: Option<i16>,
    action: String,
    kinds: Vec<String>,
    target_desc: Option<String>,
    target_kind: Option<String>,
    target_value: Option<String>,
}

impl RuleRow {
    fn max_str(&self) -> String {
        self.max_len.map(|m| m.to_string()).unwrap_or_else(|| "∞".into())
    }
    /// for the Edit button
    fn max_value(&self) -> String {
        self.max_len.map(|m| m.to_string()).unwrap_or_default()
    }
    fn kinds_list(&self) -> String {
        self.kinds.join(",")
    }
    fn target_kind_str(&self) -> &str {
        self.target_kind.as_deref().unwrap_or("none")
    }
    fn target_value_str(&self) -> &str {
        self.target_value.as_deref().unwrap_or_default()
    }
}

#[derive(Template)]
#[template(path = "admin_trunk.html")]
struct TrunkPage {
    c: Ctx,
    t: TrunkRow,
    is_new: bool,
    rules: Vec<RuleRow>,
    classes: Vec<(String, String)>,
    opts: TargetOptions,
    newrule: TargetSel,
    kinds: [&'static str; 5],
    /// (route id, name, prefix, class, position of this trunk in the rule)
    dial_rules: Vec<(i32, String, String, String, i16)>,
    codecs: &'static [&'static str],
}

async fn load(db: &PgPool, id: i32) -> Result<TrunkRow, AppError> {
    sqlx::query_as(&format!("{TRUNK_SELECT} AND id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)
}

async fn page(app: &AppState, a: &crate::auth::CurrentUser, t: TrunkRow, is_new: bool) -> Result<Response, AppError> {
    let rules = sqlx::query_as(&format!(
        "SELECT r.id, r.priority, r.match_prefix, r.min_len, r.max_len, r.action, r.kinds, {TARGET_DESC} AS target_desc,
                t.kind::text AS target_kind, {} AS target_value
         FROM trunk_inbound_rules r LEFT JOIN targets t ON t.id = r.target_id {TARGET_JOINS}
         WHERE r.trunk_id = $1 ORDER BY r.priority, length(r.match_prefix) DESC",
        crate::ui::target_value!()
    ))
    .bind(t.id)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let classes = sqlx::query_as("SELECT key, name FROM route_classes ORDER BY builtin DESC, name")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let dial_rules = sqlx::query_as(
        "SELECT r.id, r.name, r.match_prefix, r.route_class, rt.position FROM outbound_routes r
         JOIN route_trunks rt ON rt.route_id = r.id WHERE rt.trunk_id = $1 ORDER BY r.priority",
    )
    .bind(t.id)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    render(TrunkPage {
        dial_rules,
        c: Ctx::new(app, a).section("trunks"),
        t,
        is_new,
        rules,
        classes,
        opts: TargetOptions::load(&app.db).await?,
        newrule: TargetSel::new("", None, None, "no target"),
        kinds: KINDS,
        codecs: &CODECS,
    })
}

#[derive(Deserialize)]
pub struct NewQuery {
    #[serde(default)]
    kind: String,
}

pub async fn trunk_new(State(app): State<AppState>, Admin(a): Admin, axum::extract::Query(q): axum::extract::Query<NewQuery>) -> Result<Response, AppError> {
    let sip = q.kind.starts_with("sip");
    let t = TrunkRow {
        id: 0,
        name: String::new(),
        kind: if sip { "sip_register".into() } else { "iax2".into() },
        asterisk_ref: String::new(),
        enabled: true,
        managed: true,
        host: None,
        port: if sip { None } else { Some(4569) },
        has_secret: false,
        encryption: true,
        iax_trunk: false,
        calltoken: "yes".into(),
        site_prefix: None,
        iax_username: None,
        route_classes: if sip { vec![] } else { vec!["internal".into()] },
        max_channels: Some(if sip { 2 } else { 10 }),
        inbound_mode: "did".into(),
        sip_transport: "udp".into(),
        username: None,
        from_user: None,
        from_domain: None,
        outbound_proxy: None,
        media_encryption: "no".into(),
        codecs: vec!["g722".into(), "alaw".into()],
        identify_match: None,
        clip_own_only: false,
        accept_caller_name: true,
        accept_caller_number: true,
    };
    page(&app, &a, t, true).await
}

pub async fn trunk(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    page(&app, &a, t, false).await
}

#[derive(Deserialize)]
pub struct TrunkForm {
    name: String,
    #[serde(default)]
    site_prefix: String,
    #[serde(default)]
    iax_username: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    encryption: Option<String>,
    #[serde(default)]
    iax_trunk: Option<String>,
    #[serde(default)]
    calltoken: String,
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    max_channels: String,
    #[serde(default)]
    classes: Vec<String>,
    // SIP only
    #[serde(default)]
    kind: String,
    #[serde(default)]
    sip_transport: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    from_user: String,
    #[serde(default)]
    from_domain: String,
    #[serde(default)]
    outbound_proxy: String,
    #[serde(default)]
    media_encryption: String,
    #[serde(default)]
    codecs: Vec<String>,
    #[serde(default)]
    identify_match: String,
    #[serde(default)]
    inbound_mode: String,
    #[serde(default)]
    clip_own_only: Option<String>,
}

/// Values that end up in pjsip.conf: no whitespace, no comment or section characters.
fn conf_safe(label: &str, v: &str, max: usize, extra: &str) -> Result<Option<String>, AppError> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.len() > max || !v.chars().all(|c| c.is_ascii_alphanumeric() || ".-_:".contains(c) || extra.contains(c)) {
        return Err(AppError::BadRequest(format!("{label}: invalid characters.")));
    }
    Ok(Some(v.to_string()))
}

async fn sip_save(app: &AppState, a: &crate::auth::CurrentUser, id: i32, is_new: bool, f: TrunkForm) -> Result<Response, AppError> {
    let name = f.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(AppError::BadRequest("Name: 1–60 characters.".into()));
    }
    let kind = if f.kind == "sip_ip" { "sip_ip" } else { "sip_register" };
    let host = conf_safe("Server", &f.host, 100, "")?.ok_or_else(|| AppError::BadRequest("Server: host name or IP address.".into()))?;
    let port = f.port.trim().parse::<i32>().ok().filter(|p| (1..=65535).contains(p));
    let transport = if ["udp", "tcp", "tls"].contains(&f.sip_transport.as_str()) { f.sip_transport.as_str() } else { "udp" };
    let username = conf_safe("Username", &f.username, 80, "+@")?;
    let from_user = conf_safe("From user", &f.from_user, 80, "+")?;
    let from_domain = conf_safe("From domain", &f.from_domain, 100, "")?;
    let proxy = conf_safe("Outbound proxy", &f.outbound_proxy, 100, "")?;
    let ident = conf_safe("Additional sources", &f.identify_match, 300, ",/")?;
    // only hosts or narrow networks: a wide match would take over devices and other trunks
    for m in ident.as_deref().unwrap_or("").split(',').map(str::trim).filter(|m| !m.is_empty()) {
        let wide = match m.split_once('/') {
            Some((addr, len)) => {
                let len: u32 = len.parse().unwrap_or(0);
                if addr.contains(':') { len < 64 } else { len < 24 }
            }
            None => m == "0.0.0.0" || m == "::",
        };
        if wide {
            return Err(AppError::BadRequest(format!("Additional sources: {m} is too wide (at most /24).")));
        }
    }
    let media = if f.media_encryption == "sdes" { "sdes" } else { "no" };
    let mode = if f.inbound_mode == "passthrough" { "passthrough" } else { "did" };
    let codecs: Vec<String> = CODECS.iter().filter(|c| f.codecs.iter().any(|x| x == *c)).map(|c| c.to_string()).collect();
    if codecs.is_empty() {
        return Err(AppError::BadRequest("Select at least one codec.".into()));
    }
    if kind == "sip_register" && username.is_none() {
        return Err(AppError::BadRequest("Registration needs a username.".into()));
    }
    let key = crypto::load_key().map_err(AppError::Internal)?;
    let secret_enc = match f.secret.trim() {
        "" => None,
        s if s.len() > 128 || s.chars().any(|c| c.is_whitespace() || c == ';' || c == '#' || c.is_control()) => {
            return Err(AppError::BadRequest("Password: no spaces, “;” or “#”.".into()));
        }
        s => Some(crypto::encrypt(&key, s).map_err(AppError::Internal)?),
    };
    if is_new && kind == "sip_register" && secret_enc.is_none() {
        return Err(AppError::BadRequest("Registration needs a password.".into()));
    }
    let classes: Vec<String> = f.classes.iter().filter(|c| !c.is_empty()).cloned().collect();
    let max = f.max_channels.parse::<i32>().ok().filter(|m| (1..=100).contains(m));
    let q = if is_new {
        "WITH n AS (SELECT nextval(pg_get_serial_sequence('trunks', 'id')) AS id)
         INSERT INTO trunks (id, asterisk_ref, name, kind, enabled, host, port, sip_transport, username, from_user, from_domain,
                             outbound_proxy, identify_match, media_encryption, inbound_mode, codecs, route_classes, max_channels,
                             secret_enc, clip_own_only, managed)
         SELECT n.id, 'sip-' || n.id, $2, $3::trunk_kind, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::inbound_mode, $15, $16, $17, $18, $19, true
         FROM n RETURNING id"
    } else {
        "UPDATE trunks SET name = $2, kind = $3::trunk_kind, enabled = $4, host = $5, port = $6, sip_transport = $7, username = $8,
                from_user = $9, from_domain = $10, outbound_proxy = $11, identify_match = $12, media_encryption = $13,
                inbound_mode = $14::inbound_mode, codecs = $15, route_classes = $16, max_channels = $17,
                secret_enc = COALESCE($18, secret_enc), clip_own_only = $19
         WHERE id = $1 RETURNING id"
    };
    let saved: i32 = sqlx::query_scalar(q)
        .bind(id)
        .bind(&name)
        .bind(kind)
        .bind(f.enabled.is_some())
        .bind(&host)
        .bind(port)
        .bind(transport)
        .bind(&username)
        .bind(&from_user)
        .bind(&from_domain)
        .bind(&proxy)
        .bind(&ident)
        .bind(media)
        .bind(mode)
        .bind(&codecs)
        .bind(&classes)
        .bind(max)
        .bind(&secret_enc)
        .bind(f.clip_own_only.is_some())
        .fetch_one(&app.db)
        .await
        .map_err(|e| AppError::BadRequest(format!("Saving failed: {e}")))?;
    db::audit(
        &app.db,
        &a.session.sub,
        if is_new { "trunk.add" } else { "trunk.edit" },
        &format!("trunk-id:{saved}"),
        serde_json::json!({"kind": kind, "host": host, "transport": transport, "mode": mode, "classes": classes,
            "clip_own_only": f.clip_own_only.is_some(), "secret_changed": secret_enc.is_some()}),
    )
    .await?;
    if is_new && mode == "passthrough" {
        sqlx::query("INSERT INTO trunk_inbound_rules (trunk_id, priority, action, kinds) VALUES ($1, 50, 'internal', '{person,group}')")
            .bind(saved)
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
    }
    apply_sip(app).await.map_err(|e| AppError::BadRequest(format!("Saved, but activation failed: {e:#}")))?;
    Ok(Redirect::to(&format!("/admin/trunks/{saved}")).into_response())
}

/// Prefix rules: digits, not starting with 0 or 11, prefix-free against all other trunk prefixes,
/// and no collision with internal numbers (prefix + 4 digits ≥ 6 digits).
async fn check_prefix(db: &PgPool, p: &str, own_id: i32) -> Result<(), AppError> {
    if !(2..=4).contains(&p.len()) || !p.chars().all(|c| c.is_ascii_digit()) || p.starts_with('0') || p.starts_with("11") {
        return Err(AppError::BadRequest("Site prefix: 2–4 digits, not starting with 0 or 11.".into()));
    }
    let others: Vec<String> =
        sqlx::query_scalar("SELECT site_prefix FROM trunks WHERE site_prefix IS NOT NULL AND deleted_at IS NULL AND id <> $1")
            .bind(own_id)
            .fetch_all(db)
            .await
            .map_err(anyhow::Error::from)?;
    for o in others.iter().filter(|o| !o.is_empty()) {
        if o.starts_with(p) || p.starts_with(o.as_str()) {
            return Err(AppError::BadRequest(format!("Prefix {p} collides with prefix {o} (prefixes must be prefix-free).")));
        }
    }
    Ok(())
}

pub async fn trunk_save(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<String>, MultiForm(f): MultiForm<TrunkForm>) -> Result<Response, AppError> {
    let is_new = id == "new";
    let id: i32 = if is_new { 0 } else { id.parse().map_err(|_| AppError::NotFound)? };
    let old = if is_new { None } else { Some(load(&app.db, id).await?) };
    if let Some(o) = &old
        && !o.managed
    {
        // trunks defined outside patchbay-web: only permissions and channel limit are editable
        let classes: Vec<String> = f.classes.iter().filter(|c| !c.is_empty()).cloned().collect();
        sqlx::query("UPDATE trunks SET route_classes = $2, max_channels = $3 WHERE id = $1")
            .bind(id)
            .bind(&classes)
            .bind(f.max_channels.parse::<i32>().ok())
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        db::audit(&app.db, &a.session.sub, "trunk.classes", &format!("trunk:{}", o.asterisk_ref), serde_json::json!({"classes": classes})).await?;
        return Ok(Redirect::to(&format!("/admin/trunks/{id}")).into_response());
    }
    let sip = match &old {
        Some(o) => o.is_sip(),
        None => f.kind.starts_with("sip"),
    };
    if sip {
        return sip_save(&app, &a, id, is_new, f).await;
    }
    let prefix = f.site_prefix.trim().to_string();
    check_prefix(&app.db, &prefix, id).await?;
    let login = f.iax_username.trim().to_string();
    if login.is_empty() || login.len() > 40 || !login.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)) {
        return Err(AppError::BadRequest("Our login at the peer: 1–40 letters, digits, _ or -.".into()));
    }
    let host = f.host.trim().to_string();
    if host.is_empty() || host.len() > 100 || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c)) {
        return Err(AppError::BadRequest("Host: IP address or DNS name.".into()));
    }
    let port: i32 = f.port.trim().parse().unwrap_or(4569);
    if !(1..=65535).contains(&port) {
        return Err(AppError::BadRequest("Port: 1–65535.".into()));
    }
    let name = f.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(AppError::BadRequest("Name: 1–60 characters.".into()));
    }
    let key = crypto::load_key().map_err(AppError::Internal)?;
    let secret_enc = match f.secret.trim() {
        "" if is_new => return Err(AppError::BadRequest("A shared secret is required (same on both sides).".into())),
        "" => None,
        s if s.len() < 16 || s.contains(char::is_whitespace) || s.contains(';') => {
            return Err(AppError::BadRequest("Secret: at least 16 characters, no spaces or semicolons.".into()));
        }
        s => Some(crypto::encrypt(&key, s).map_err(AppError::Internal)?),
    };
    let classes: Vec<String> = f.classes.iter().filter(|c| !c.is_empty()).cloned().collect();
    let asterisk_ref = format!("site{prefix}");
    let calltoken = if f.calltoken == "auto" { "auto" } else { "yes" };
    let max = f.max_channels.parse::<i32>().ok().filter(|m| (1..=100).contains(m));
    let saved: i32 = if is_new {
        sqlx::query_scalar(
            "INSERT INTO trunks (name, kind, asterisk_ref, enabled, inbound_mode, managed, host, port, secret_enc, encryption,
                                 iax_trunk, calltoken, site_prefix, route_classes, max_channels, iax_username)
             VALUES ($1, 'iax2', $2, $3, 'passthrough', true, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
        )
        .bind(&name)
        .bind(&asterisk_ref)
        .bind(f.enabled.is_some())
        .bind(&host)
        .bind(port)
        .bind(&secret_enc)
        .bind(f.encryption.is_some())
        .bind(f.iax_trunk.is_some())
        .bind(calltoken)
        .bind(&prefix)
        .bind(&classes)
        .bind(max)
        .bind(&login)
        .fetch_one(&app.db)
        .await
    } else {
        sqlx::query_scalar(
            "UPDATE trunks SET name = $2, asterisk_ref = $3, enabled = $4, host = $5, port = $6,
                    secret_enc = COALESCE($7, secret_enc), encryption = $8, iax_trunk = $9, calltoken = $10,
                    site_prefix = $11, route_classes = $12, max_channels = $13, iax_username = $14
             WHERE id = $1 RETURNING id",
        )
        .bind(id)
        .bind(&name)
        .bind(&asterisk_ref)
        .bind(f.enabled.is_some())
        .bind(&host)
        .bind(port)
        .bind(&secret_enc)
        .bind(f.encryption.is_some())
        .bind(f.iax_trunk.is_some())
        .bind(calltoken)
        .bind(&prefix)
        .bind(&classes)
        .bind(max)
        .bind(&login)
        .fetch_one(&app.db)
        .await
    }
    .map_err(|e| AppError::BadRequest(format!("Saving failed (name/prefix already used?): {e}")))?;
    db::audit(
        &app.db,
        &a.session.sub,
        if is_new { "trunk.add" } else { "trunk.edit" },
        &format!("trunk:{asterisk_ref}"),
        serde_json::json!({"host": host, "prefix": prefix, "classes": classes, "secret_changed": secret_enc.is_some()}),
    )
    .await?;
    if is_new {
        // Standard inbound rule: internal numbers (persons and call groups)
        sqlx::query("INSERT INTO trunk_inbound_rules (trunk_id, priority, action, kinds) VALUES ($1, 50, 'internal', '{person,group}')")
            .bind(saved)
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
    }
    apply(&app).await.map_err(|e| AppError::BadRequest(format!("Saved, but activation failed: {e:#}")))?;
    Ok(Redirect::to(&format!("/admin/trunks/{saved}")).into_response())
}

/// Creates the standard dial rule for an IAX peer: <prefix> + ≥4 digits, strip the prefix,
/// class site<prefix> (created if missing).
pub async fn standard_route(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    let prefix = t.site_prefix.clone().ok_or_else(|| AppError::BadRequest("Trunk has no site prefix.".into()))?;
    let class = format!("site{prefix}");
    sqlx::query(
        "INSERT INTO route_classes (key, name, description) VALUES ($1, $2, $3) ON CONFLICT (key) DO NOTHING",
    )
    .bind(&class)
    .bind(format!("Site {prefix}"))
    .bind(format!("Calls to {} via IAX", t.name))
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM outbound_routes r JOIN route_trunks rt ON rt.route_id = r.id
                        WHERE rt.trunk_id = $1 AND r.match_prefix = $2)",
    )
    .bind(id)
    .bind(&prefix)
    .fetch_one(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    if !exists {
        sqlx::query(
            "WITH r AS (INSERT INTO outbound_routes (priority, name, match_prefix, min_len, action, strip_digits, clip_mode, route_class)
                        VALUES (20, $1, $2, $3, 'route', $5, 'route', $6) RETURNING id)
             INSERT INTO route_trunks (route_id, position, trunk_id) SELECT r.id, 1, $4 FROM r",
        )
        .bind(format!("Site {prefix} ({})", t.name))
        .bind(&prefix)
        .bind((prefix.len() + 4) as i16)
        .bind(id)
        .bind(prefix.len() as i16)
        .bind(&class)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    }
    db::audit(&app.db, &a.session.sub, "trunk.standard_route", &format!("trunk:{}", t.asterisk_ref), serde_json::json!({"class": class})).await?;
    Ok(Redirect::to("/admin/classes").into_response())
}

#[derive(Deserialize)]
pub struct RuleForm {
    priority: i16,
    #[serde(default)]
    match_prefix: String,
    min_len: i16,
    #[serde(default)]
    max_len: String,
    action: String,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    value: String,
    /// set when editing a rule (Edit button)
    #[serde(default)]
    id: String,
}

/// Adds an inbound rule, or changes one (`id`, Edit button).
pub async fn rule_add(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, MultiForm(f): MultiForm<RuleForm>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    if !["internal", "target", "outbound", "reject"].contains(&f.action.as_str()) {
        return Err(AppError::BadRequest("Unknown action.".into()));
    }
    if !f.match_prefix.chars().all(|c| c.is_ascii_digit()) || f.match_prefix.len() > 10 || !(1..=20).contains(&f.min_len) {
        return Err(AppError::BadRequest("Pattern: digits only; min. length 1–20.".into()));
    }
    let kinds: Vec<String> = f.kinds.iter().filter(|k| KINDS.contains(&k.as_str())).cloned().collect();
    let max_len = f.max_len.trim().parse::<i16>().ok();
    let rid: Option<i32> = match f.id.trim() {
        "" => None,
        r => Some(r.parse().map_err(|_| AppError::BadRequest("Invalid rule.".into()))?),
    };
    // the rule's current target (editing): reused for a target, dropped otherwise
    let old_target: Option<i32> = match rid {
        Some(rid) => sqlx::query_scalar::<_, Option<i32>>("SELECT target_id FROM trunk_inbound_rules WHERE trunk_id = $1 AND id = $2")
            .bind(id)
            .bind(rid)
            .fetch_optional(&app.db)
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| AppError::BadRequest("The rule no longer exists. Please reload the page.".into()))?,
        None => None,
    };
    // the same rule twice would never be reached and only confuses (deleting one changes nothing)
    if f.action != "target" {
        let same: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM trunk_inbound_rules WHERE trunk_id = $1 AND priority = $2
               AND COALESCE(match_prefix, '') = $3 AND min_len = $4 AND max_len IS NOT DISTINCT FROM $5
               AND action::text = $6 AND kinds = $7 AND id IS DISTINCT FROM $8)",
        )
        .bind(id)
        .bind(f.priority)
        .bind(&f.match_prefix)
        .bind(f.min_len)
        .bind(max_len)
        .bind(&f.action)
        .bind(&kinds)
        .bind(rid)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
        if same {
            return Err(AppError::BadRequest("This rule exists already.".into()));
        }
    }
    let target = if f.action == "target" {
        Some(save_target(&app.db, old_target, &TargetInput { kind: f.kind.clone(), value: f.value.clone() }).await?.ok_or_else(|| AppError::BadRequest("Select a target.".into()))?)
    } else {
        None
    };
    let n = sqlx::query(if rid.is_some() {
        "UPDATE trunk_inbound_rules SET priority = $2, match_prefix = $3, min_len = $4, max_len = $5, action = $6,
                kinds = $7, target_id = $8 WHERE trunk_id = $1 AND id = $9"
    } else {
        "INSERT INTO trunk_inbound_rules (trunk_id, priority, match_prefix, min_len, max_len, action, kinds, target_id)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8 WHERE $9::int IS NULL"
    })
    .bind(id)
    .bind(f.priority)
    .bind(&f.match_prefix)
    .bind(f.min_len)
    .bind(max_len)
    .bind(&f.action)
    .bind(&kinds)
    .bind(target)
    .bind(rid)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("The rule no longer exists. Please reload the page.".into()));
    }
    // a target the rule no longer uses (action changed away from "target")
    if let (Some(old), None) = (old_target, target) {
        sqlx::query("DELETE FROM targets WHERE id = $1").bind(old).execute(&app.db).await.map_err(anyhow::Error::from)?;
    }
    let action = if rid.is_some() { "trunk.rule_edit" } else { "trunk.rule_add" };
    db::audit(&app.db, &a.session.sub, action, &format!("trunk:{}", t.asterisk_ref), serde_json::json!({"rule": rid, "action": f.action, "prefix": f.match_prefix, "kinds": kinds})).await?;
    Ok(Redirect::to(&format!("/admin/trunks/{id}")).into_response())
}

pub async fn rule_delete(State(app): State<AppState>, Admin(a): Admin, Path((id, rid)): Path<(i32, i32)>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM trunk_inbound_rules WHERE trunk_id = $1 AND id = $2").bind(id).bind(rid).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "trunk.rule_delete", &format!("trunk-id:{id}"), serde_json::json!({"rule": rid})).await?;
    Ok(Redirect::to(&format!("/admin/trunks/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct CallerIdForm {
    #[serde(default)]
    accept_caller_name: Option<String>,
    #[serde(default)]
    accept_caller_number: Option<String>,
}

/// POST /admin/trunks/{id}/caller-id: take over the caller name / number of incoming calls
pub async fn caller_id(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, axum::Form(f): axum::Form<CallerIdForm>) -> Result<Response, AppError> {
    let (name, number) = (f.accept_caller_name.is_some(), f.accept_caller_number.is_some());
    let n = sqlx::query("UPDATE trunks SET accept_caller_name = $2, accept_caller_number = $3 WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .bind(name)
        .bind(number)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if n.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    db::audit(&app.db, &a.session.sub, "trunk.caller_id", &format!("trunk-id:{id}"), serde_json::json!({"name": name, "number": number})).await?;
    Ok(Redirect::to(&format!("/admin/trunks/{id}#caller-id")).into_response())
}

pub async fn trunk_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    if !t.managed {
        return Err(AppError::BadRequest("This trunk is not managed in the web interface.".into()));
    }
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM route_trunks WHERE trunk_id = $1")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if used > 0 {
        return Err(AppError::BadRequest("The trunk is still used by dial rules.".into()));
    }
    sqlx::query("UPDATE trunks SET deleted_at = now(), enabled = false, site_prefix = NULL WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "trunk.delete", &format!("trunk:{}", t.asterisk_ref), serde_json::json!({})).await?;
    apply_all(&app).await.map_err(|e| AppError::BadRequest(format!("Deleted, but reload failed: {e:#}")))?;
    Ok(Redirect::to("/admin/trunks").into_response())
}

/// POST /admin/trunks/{id}/register: registers the trunk again right away (AMI PJSIPRegister),
/// e.g. after the provider unlocked it, instead of waiting for the next retry (up to 10 minutes
/// after a rejection). Only this trunk's registration; calls and devices are not affected.
pub async fn trunk_register(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    let r = t.asterisk_ref.clone();
    let regs = ami::outbound_registrations().await.map_err(AppError::Internal)?;
    let Some(before) = regs.get(&r).cloned().filter(|_| t.kind == "sip_register") else {
        let msg = "This trunk has no registration (only enabled SIP trunks with registration).";
        return Ok(axum::Json(serde_json::json!({"ok": false, "message": msg})).into_response());
    };
    let resp = ami::action(&[("Action", "PJSIPRegister"), ("Registration", &r)]).await.map_err(AppError::Internal)?;
    if resp.get("Response").map(String::as_str) != Some("Success") {
        let why = resp.get("Message").cloned().unwrap_or_default();
        return Ok(axum::Json(serde_json::json!({"ok": false, "message": format!("Registration not started: {why}")})).into_response());
    }
    db::audit(&app.db, &a.session.sub, "trunk.register", &format!("trunk:{r}"), serde_json::json!({})).await?;
    // the provider usually answers within a second or two. The action returns before the new
    // attempt has started, so an unchanged final status only counts after 2 s (it may still be
    // the old one); a changed one counts at once. At most 5 s.
    let mut status = String::new();
    for i in 1..=10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        status = ami::outbound_registrations().await.map_err(AppError::Internal)?.get(&r).cloned().unwrap_or_default();
        if (status == "Registered" || status == "Rejected") && (status != before || i >= 4) {
            break;
        }
    }
    let ok = status == "Registered";
    Ok(axum::Json(serde_json::json!({"ok": ok, "message": format!("Registration: {}", if status.is_empty() { "unknown" } else { &status })})).into_response())
}

/// Ping a peer (iax2 show peer → status / latency)
pub async fn trunk_test(State(app): State<AppState>, Admin(_): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let t = load(&app.db, id).await?;
    if t.is_sip() {
        let r = t.asterisk_ref.as_str();
        let regs = ami::outbound_registrations().await.map_err(AppError::Internal)?;
        // contact status with RTT of the static AOR contact: CLI output (PJSIPShowContacts covers
        // only dynamic contacts in some versions)
        let aor = ami::command(&format!("pjsip show aor {r}")).await.map_err(AppError::Internal)?;
        let contact = aor.lines().find(|l| l.trim_start().starts_with("Contact:") && l.contains(r) && !l.contains("<Aor"));
        let mut msg = vec![];
        let mut ok = true;
        if let Some(st) = regs.get(r) {
            ok &= st == "Registered";
            msg.push(format!("Registration: {st}"));
        } else if t.kind == "sip_register" {
            ok = false;
            msg.push("Registration: unknown".into());
        }
        if let Some(l) = contact {
            let parts: Vec<&str> = l.split_whitespace().collect();
            let st = parts.iter().rev().nth(1).copied().unwrap_or("?");
            let rtt = parts.last().copied().unwrap_or("");
            ok &= st == "Avail";
            msg.push(format!("Server: {st} ({rtt} ms)"));
        }
        return Ok(axum::Json(serde_json::json!({"ok": ok, "message": msg.join(", ")})).into_response());
    }
    let peers = ami::iax_peers().await.map_err(AppError::Internal)?;
    let status = format!("Status: {}", peers.get(&t.asterisk_ref).map(String::as_str).unwrap_or("unknown"));
    Ok(axum::Json(serde_json::json!({"ok": status.contains("OK"), "message": status})).into_response())
}

pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if let Err(e) = apply(&app).await {
            tracing::warn!("iax peers: {e:#}");
        }
        if let Err(e) = apply_sip(&app).await {
            tracing::warn!("sip trunks: {e:#}");
        }
    });
}

/// POST /admin/trunks/new (the static "new" route would otherwise shadow /{id} and answer 405)
pub async fn trunk_create(state: State<AppState>, admin: Admin, form: MultiForm<TrunkForm>) -> Result<Response, AppError> {
    trunk_save(state, admin, Path("new".to_string()), form).await
}

/// Writes via a temp file + rename, so Asterisk never reads a half-written file.
async fn write_atomic(path: &str, content: &str) -> anyhow::Result<()> {
    let tmp = format!("{path}.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
