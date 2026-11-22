// SPDX-License-Identifier: GPL-3.0-or-later
//! Monitoring for external tools: `GET /monitor` in the Checkmk local check format, `GET /metrics`
//! in the Prometheus text format. Only for clients in PBX_MONITOR_NETS (default: this host, so the
//! Checkmk agent on the PBX host can fetch it); others get 404. `/monitor` answers 503 when a check
//! is critical, so an agentless HTTP check works as well.

use std::{fmt::Write, net::SocketAddr, time::Duration};

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDateTime, Utc};

use crate::{AppState, ami};

const TIMEOUT: Duration = Duration::from_secs(5);
/// certificate: warn/crit below this many days
const CERT_WARN_DAYS: i64 = 21;
const CERT_CRIT_DAYS: i64 = 7;

#[derive(Debug, Default)]
struct Status {
    db_ok: bool,
    db_error: String,
    asterisk_uptime: Option<u64>,
    asterisk_error: String,
    /// name, kind, asterisk_ref, up (None = not monitored: disabled or not qualified), status text
    trunks: Vec<(String, String, String, Option<bool>, String)>,
    cert_days: Option<i64>,
    cert_error: String,
    devices_total: i64,
    devices_registered: i64,
    channels: i64,
    /// wrong app PINs (dial-through, conference) in the last hour (audit log, app.pin_failed)
    pin_failures: i64,
    /// apps whose PIN was locked within the last 15 minutes ("app:<number>"), i.e. probably still are
    pin_locked: Vec<String>,
    /// failed logins at Asterisk in the last hour (secwatch.rs)
    sip_auth: crate::secwatch::Summary,
}

fn allowed(app: &AppState, peer: SocketAddr, headers: &HeaderMap) -> bool {
    let ip = crate::proxy::client_ip(&app.cfg.trusted_proxies, peer, headers);
    app.cfg.monitor_nets.iter().any(|n| n.contains(ip))
}

async fn collect(app: &AppState) -> Status {
    let mut s = Status { sip_auth: crate::secwatch::summary(), ..Default::default() };
    match tokio::time::timeout(TIMEOUT, sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&app.db)).await {
        Ok(Ok(_)) => s.db_ok = true,
        Ok(Err(e)) => s.db_error = e.to_string(),
        Err(_) => s.db_error = "timeout".into(),
    }
    if s.db_ok {
        // the dialplan locks an app for 15 minutes (sub-pinfail); the lock itself is in astdb
        let r: Option<(i64, Vec<String>)> = sqlx::query_as(
            "SELECT count(*),
                    COALESCE(array_agg(DISTINCT object ORDER BY object) FILTER (WHERE details->>'locked' = 'true' AND at > now() - interval '15 minutes'), '{}')
             FROM audit_log WHERE action = 'app.pin_failed' AND at > now() - interval '1 hour'",
        )
        .fetch_one(&app.db)
        .await
        .ok();
        if let Some((n, locked)) = r {
            s.pin_failures = n;
            s.pin_locked = locked;
        }
    }
    match tokio::time::timeout(TIMEOUT, ami::uptime_seconds()).await {
        Ok(Ok(u)) => s.asterisk_uptime = Some(u),
        Ok(Err(e)) => s.asterisk_error = format!("{e:#}"),
        Err(_) => s.asterisk_error = "timeout".into(),
    }
    if s.asterisk_uptime.is_some() {
        let regs = tokio::time::timeout(TIMEOUT, ami::outbound_registrations()).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let peers = tokio::time::timeout(TIMEOUT, ami::iax_peers()).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let contacts = tokio::time::timeout(TIMEOUT, ami::contacts()).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        if s.db_ok {
            let trunks: Vec<(String, String, String, bool)> = sqlx::query_as(
                "SELECT name, kind::text, asterisk_ref, enabled FROM trunks WHERE deleted_at IS NULL ORDER BY name",
            )
            .fetch_all(&app.db)
            .await
            .unwrap_or_default();
            for (name, kind, r, enabled) in trunks {
                let (up, text) = match kind.as_str() {
                    _ if !enabled => (None, "disabled".into()),
                    "sip_register" => {
                        let st = regs.get(&r).cloned().unwrap_or_else(|| "not loaded".into());
                        (Some(st == "Registered"), st)
                    }
                    "iax2" => {
                        let st = peers.get(&r).cloned().unwrap_or_else(|| "not loaded".into());
                        if st == "Unmonitored" { (None, st) } else { (Some(st.starts_with("OK")), st) }
                    }
                    // IP-based SIP trunk: only known when Asterisk qualifies it
                    _ => match contacts.get(&r) {
                        Some(c) if c.status == "Reachable" || c.status == "Avail" => (Some(true), c.status.clone()),
                        Some(c) if !c.status.is_empty() && c.status != "Unknown" => (Some(false), c.status.clone()),
                        _ => (None, "not qualified".into()),
                    },
                };
                s.trunks.push((name, kind, r, up, text));
            }
            let devices: Vec<String> = sqlx::query_scalar("SELECT sip_username FROM active_devices").fetch_all(&app.db).await.unwrap_or_default();
            s.devices_total = devices.len() as i64;
            s.devices_registered = devices.iter().filter(|d| contacts.contains_key(*d)).count() as i64;
        }
        s.channels = tokio::time::timeout(TIMEOUT, ami::list("CoreShowChannels", "CoreShowChannel"))
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| v.len() as i64)
            .unwrap_or(0);
    }
    if let Some(path) = app.cfg.asterisk_cert.clone().or_else(|| app.cfg.tls.as_ref().map(|t| t.0.clone())) {
        match cert_days(&path).await {
            Ok(d) => s.cert_days = Some(d),
            Err(e) => s.cert_error = format!("{path}: {e:#}"),
        }
    }
    s
}

/// Days until the certificate expires (openssl is in the image anyway).
async fn cert_days(path: &str) -> anyhow::Result<i64> {
    let out = tokio::process::Command::new("openssl").args(["x509", "-noout", "-enddate", "-in", path]).output().await?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    let s = String::from_utf8_lossy(&out.stdout);
    let end = parse_enddate(&s).ok_or_else(|| anyhow::anyhow!("unexpected openssl output: {}", s.trim()))?;
    Ok((end - Utc::now().naive_utc()).num_days())
}

/// "notAfter=Feb 18 12:00:00 2027 GMT"
fn parse_enddate(s: &str) -> Option<NaiveDateTime> {
    let v = s.trim().strip_prefix("notAfter=")?.trim().strip_suffix("GMT")?.trim();
    let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
    NaiveDateTime::parse_from_str(&v, "%b %d %H:%M:%S %Y").ok()
}

/// Checkmk service names: no quotes or line breaks
fn svc(name: &str) -> String {
    name.chars().filter(|c| *c != '"' && !c.is_control()).collect()
}

/// One check: URL key (/monitor/<key>), Checkmk state 0/1/2, service name, metrics, text
struct Check {
    key: String,
    state: u8,
    name: String,
    metrics: String,
    text: String,
}

fn checks(s: &Status) -> Vec<Check> {
    let mut v = vec![];
    let mut add = |key: &str, state: u8, name: &str, metrics: &str, text: &str| {
        v.push(Check {
            key: key.to_string(),
            state,
            name: svc(name),
            metrics: metrics.to_string(),
            text: text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect(),
        })
    };
    if s.db_ok {
        add("database", 0, "PBX Database", "", "PostgreSQL reachable");
    } else {
        add("database", 2, "PBX Database", "", &format!("PostgreSQL not reachable: {}", s.db_error));
    }
    match s.asterisk_uptime {
        Some(u) => add("asterisk", 0, "PBX Asterisk", &format!("uptime={u}"), &format!("Asterisk running for {}, patchbay {}", human(u), crate::config::build_version())),
        None => add("asterisk", 2, "PBX Asterisk", "", &format!("Asterisk Manager not reachable: {}", s.asterisk_error)),
    }
    for (name, _kind, r, up, text) in &s.trunks {
        let key = format!("trunk/{r}");
        match up {
            Some(true) => add(&key, 0, &format!("PBX Trunk {name}"), "", text),
            Some(false) => add(&key, 2, &format!("PBX Trunk {name}"), "", text),
            None => add(&key, 0, &format!("PBX Trunk {name}"), "", &format!("{text} (not monitored)")),
        }
    }
    match s.cert_days {
        Some(d) => {
            let st = if d < CERT_CRIT_DAYS { 2 } else if d < CERT_WARN_DAYS { 1 } else { 0 };
            add("certificate", st, "PBX Certificate", &format!("days_left={d};{CERT_WARN_DAYS};{CERT_CRIT_DAYS}"), &format!("expires in {d} days"));
        }
        None if !s.cert_error.is_empty() => add("certificate", 2, "PBX Certificate", "", &s.cert_error),
        None => {}
    }
    if s.asterisk_uptime.is_some() && s.db_ok {
        add("devices", 0, "PBX Devices", &format!("registered={}|total={}", s.devices_registered, s.devices_total), &format!("{} of {} devices registered", s.devices_registered, s.devices_total));
        add("calls", 0, "PBX Calls", &format!("channels={}", s.channels), &format!("{} active channels", s.channels));
    }
    if s.asterisk_uptime.is_some() {
        let a = &s.sip_auth;
        let metrics = format!(
            "failures_1h={};{};{}|sources_1h={};;{}",
            a.failures,
            crate::secwatch::WARN_FAILURES,
            crate::secwatch::CRIT_FAILURES,
            a.sources,
            crate::secwatch::CRIT_SOURCES
        );
        let text = if a.failures == 0 {
            "no failed logins in the last hour".to_string()
        } else {
            format!("{} failed logins from {} sources in the last hour: {}", a.failures, a.sources, a.top.join("; "))
        };
        add("sip-auth", crate::secwatch::state(a), "PBX SIP Logins", &metrics, &text);
    }
    if s.db_ok {
        let metrics = format!("failures_1h={}|locked={}", s.pin_failures, s.pin_locked.len());
        if s.pin_locked.is_empty() {
            add("pins", 0, "PBX App PINs", &metrics, &format!("{} wrong app PINs in the last hour", s.pin_failures));
        } else {
            let text = format!("PIN locked after repeated wrong PINs: {} ({} wrong PINs in the last hour)", s.pin_locked.join(", "), s.pin_failures);
            add("pins", 1, "PBX App PINs", &metrics, &text);
        }
    }
    v
}

fn line(c: &Check) -> String {
    format!("{} \"{}\" {} {}\n", c.state, c.name, if c.metrics.is_empty() { "-" } else { &c.metrics }, c.text)
}

fn checkmk_text(s: &Status) -> (String, bool) {
    let cs = checks(s);
    (cs.iter().map(line).collect(), cs.iter().any(|c| c.state == 2))
}

fn human(secs: u64) -> String {
    match secs {
        s if s >= 86400 => format!("{}d {}h", s / 86400, s % 86400 / 3600),
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s => format!("{}m", s / 60),
    }
}

fn label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn prometheus_text(s: &Status) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# HELP pbx_build_info Running version (label version).\n# TYPE pbx_build_info gauge");
    let _ = writeln!(o, "pbx_build_info{{version=\"{}\"}} 1", label(crate::config::build_version()));
    let _ = writeln!(o, "# HELP pbx_up Component reachable (1) or not (0).\n# TYPE pbx_up gauge");
    let _ = writeln!(o, "pbx_up{{component=\"database\"}} {}", s.db_ok as u8);
    let _ = writeln!(o, "pbx_up{{component=\"asterisk\"}} {}", s.asterisk_uptime.is_some() as u8);
    if let Some(u) = s.asterisk_uptime {
        let _ = writeln!(o, "# TYPE pbx_asterisk_uptime_seconds gauge\npbx_asterisk_uptime_seconds {u}");
    }
    let _ = writeln!(o, "# HELP pbx_trunk_up Trunk registered/reachable (1) or not (0); unmonitored trunks are left out.\n# TYPE pbx_trunk_up gauge");
    for (name, kind, _, up, _) in &s.trunks {
        if let Some(up) = up {
            let _ = writeln!(o, "pbx_trunk_up{{trunk=\"{}\",kind=\"{}\"}} {}", label(name), label(kind), *up as u8);
        }
    }
    if let Some(d) = s.cert_days {
        let _ = writeln!(o, "# TYPE pbx_certificate_days_left gauge\npbx_certificate_days_left {d}");
    }
    let _ = writeln!(o, "# TYPE pbx_devices_total gauge\npbx_devices_total {}", s.devices_total);
    let _ = writeln!(o, "# TYPE pbx_devices_registered gauge\npbx_devices_registered {}", s.devices_registered);
    let _ = writeln!(o, "# TYPE pbx_active_channels gauge\npbx_active_channels {}", s.channels);
    let _ = writeln!(o, "# HELP pbx_app_pin_failures_1h Wrong app PINs (dial-through, conference) in the last hour.\n# TYPE pbx_app_pin_failures_1h gauge");
    let _ = writeln!(o, "pbx_app_pin_failures_1h {}", s.pin_failures);
    let _ = writeln!(o, "# HELP pbx_app_pins_locked Apps whose PIN was locked within the last 15 minutes.\n# TYPE pbx_app_pins_locked gauge");
    let _ = writeln!(o, "pbx_app_pins_locked {}", s.pin_locked.len());
    let _ = writeln!(o, "# HELP pbx_sip_auth_failures_1h Failed logins at Asterisk (wrong password, unknown account, ACL) in the last hour.\n# TYPE pbx_sip_auth_failures_1h gauge");
    let _ = writeln!(o, "pbx_sip_auth_failures_1h {}", s.sip_auth.failures);
    let _ = writeln!(o, "# HELP pbx_sip_auth_sources_1h Distinct source addresses of those failed logins.\n# TYPE pbx_sip_auth_sources_1h gauge");
    let _ = writeln!(o, "pbx_sip_auth_sources_1h {}", s.sip_auth.sources);
    o
}

pub async fn checkmk(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    if !allowed(&app, peer, &headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (body, crit) = checkmk_text(&collect(&app).await);
    let code = if crit { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (code, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("<<<local:sep(0)>>>\n{body}")).into_response()
}

/// One check, for agentless monitoring with one HTTP service per check (Checkmk "Check HTTP web
/// service" with one endpoint per URL): 200 = OK/WARN, 503 = CRIT, 404 = unknown check. The body
/// is the check's line, e.g. `/monitor/trunk/provider-054`, `/monitor/certificate`.
pub async fn checkmk_one(
    State(app): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Response {
    if !allowed(&app, peer, &headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(c) = checks(&collect(&app).await).into_iter().find(|c| c.key == key) else {
        return (StatusCode::NOT_FOUND, "unknown check\n").into_response();
    };
    let code = if c.state == 2 { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (code, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], line(&c)).into_response()
}

pub async fn prometheus(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    if !allowed(&app, peer, &headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = prometheus_text(&collect(&app).await);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enddate() {
        let d = parse_enddate("notAfter=Feb  8 12:00:00 2027 GMT\n").unwrap();
        assert_eq!(d.to_string(), "2027-02-08 12:00:00");
        assert!(parse_enddate("garbage").is_none());
    }

    #[test]
    fn checkmk_format() {
        let s = Status {
            db_ok: true,
            asterisk_uptime: Some(7200),
            trunks: vec![
                ("provider \"054\"".into(), "sip_register".into(), "provider-054".into(), Some(true), "Registered".into()),
                ("Site B".into(), "iax2".into(), "site124".into(), Some(false), "UNREACHABLE".into()),
                ("provider 053".into(), "sip_register".into(), "sip-5".into(), None, "disabled".into()),
            ],
            cert_days: Some(10),
            devices_total: 3,
            devices_registered: 2,
            channels: 1,
            pin_failures: 6,
            pin_locked: vec!["app:8100".into()],
            ..Default::default()
        };
        let (t, crit) = checkmk_text(&s);
        assert!(t.contains("1 \"PBX App PINs\" failures_1h=6|locked=1 PIN locked after repeated wrong PINs: app:8100 (6 wrong PINs in the last hour)\n"), "{t}");
        assert!(crit);
        assert!(t.contains("0 \"PBX Trunk provider 054\" - Registered\n"));
        assert!(t.contains("2 \"PBX Trunk Site B\" - UNREACHABLE\n"));
        assert!(t.contains("0 \"PBX Trunk provider 053\" - disabled (not monitored)\n"));
        let cs = checks(&s);
        assert_eq!(cs.iter().find(|c| c.key == "trunk/site124").unwrap().state, 2);
        assert_eq!(cs.iter().find(|c| c.key == "certificate").unwrap().state, 1);
        assert!(t.contains("1 \"PBX Certificate\" days_left=10;21;7 expires in 10 days\n"));
        assert!(t.contains("0 \"PBX Devices\" registered=2|total=3 2 of 3 devices registered\n"));
        let p = prometheus_text(&s);
        assert!(p.contains("pbx_trunk_up{trunk=\"provider \\\"054\\\"\",kind=\"sip_register\"} 1"));
        assert!(p.contains("pbx_trunk_up{trunk=\"Site B\",kind=\"iax2\"} 0"));
        assert!(p.contains("\npbx_app_pin_failures_1h 6\n") && p.contains("\npbx_app_pins_locked 1\n"));
    }
}
