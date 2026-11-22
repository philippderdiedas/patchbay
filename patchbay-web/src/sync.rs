// SPDX-License-Identifier: GPL-3.0-or-later
//! Keycloak sync job: refreshes phone permission and Keycloak groups of all linked users,
//! so that leaving a group (or a disabled account) takes effect without waiting for a login.
//! Uses the client's service account (realm-management: view-users, query-users, query-groups).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{AppState, ami, db};

const INTERVAL: Duration = Duration::from_secs(600);

/// One run at a time (timer and the admin's "sync now" button).
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Result of one sync run.
#[derive(Default)]
pub struct Report {
    pub checked: usize,
    pub granted: usize,
    pub revoked: usize,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct KcUser {
    #[serde(default)]
    enabled: bool,
}

#[derive(Deserialize)]
struct KcGroup {
    name: String,
}

pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        // Give Keycloak and the DB a moment after startup
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            match run(&app).await {
                Ok(r) => tracing::info!("keycloak sync: {} users checked, {} granted, {} revoked", r.checked, r.granted, r.revoked),
                Err(e) => tracing::warn!("keycloak sync failed, nothing changed: {e:#}"),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// OIDC settings and HTTP client, only with PBX_OIDC_SYNC=keycloak.
async fn keycloak(app: &AppState) -> Result<(&crate::config::OidcConfig, &reqwest::Client)> {
    if !app.cfg.kc_sync {
        bail!("Keycloak integration is off (PBX_OIDC_SYNC / PBX_AUTH)");
    }
    let cfg = app.cfg.oidc.as_ref().context("OIDC not configured")?;
    Ok((cfg, &crate::auth::oidc(app).await?.http))
}

pub async fn token(app: &AppState) -> Result<String> {
    let (cfg, http) = keycloak(app).await?;
    let url = format!("{}/protocol/openid-connect/token", cfg.issuer);
    let resp = http
        .post(url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("token request: HTTP {} (service account enabled?)", resp.status());
    }
    Ok(resp.json::<TokenResponse>().await?.access_token)
}

/// Admin API base: https://host/realms/x -> https://host/admin/realms/x
fn admin_base(issuer: &str) -> String {
    issuer.replacen("/realms/", "/admin/realms/", 1)
}

/// Returns Some((enabled, group names)) or None if the user no longer exists.
async fn fetch_user(app: &AppState, token: &str, sub: &str) -> Result<Option<(bool, Vec<String>)>> {
    let (cfg, http) = keycloak(app).await?;
    let base = admin_base(&cfg.issuer);
    let resp = http.get(format!("{base}/users/{sub}")).bearer_auth(token).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("GET user: HTTP {} (role view-users missing?)", resp.status());
    }
    let user: KcUser = resp.json().await?;
    let resp = http
        .get(format!("{base}/users/{sub}/groups?briefRepresentation=true&max=1000"))
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("GET user groups: HTTP {} (role query-groups missing?)", resp.status());
    }
    let groups: Vec<KcGroup> = resp.json().await?;
    Ok(Some((user.enabled, groups.into_iter().map(|g| g.name).collect())))
}

/// One sync run. Any API error aborts the run without changing anything (a Keycloak outage
/// must not lock everybody out); changes are applied per user only after a successful fetch.
pub async fn run(app: &AppState) -> Result<Report> {
    let _guard = RUNNING.lock().await;
    let token = token(app).await.context("service account token")?;
    // PBX_PROVISION=auto: persons for members of the phone group that have none yet
    if app.cfg.provision == crate::config::Provision::Auto {
        let members = group_members(app, &token, &app.cfg.group_phone).await.context("phone group members")?;
        let n = crate::provision::sync_members(app, &members).await?;
        if n > 0 {
            tracing::info!("keycloak sync: {n} persons provisioned");
        }
    }
    let users: Vec<(i32, String, bool)> =
        sqlx::query_as("SELECT id, kc_subject, phone_allowed FROM users WHERE kc_subject IS NOT NULL AND deleted_at IS NULL")
            .fetch_all(&app.db)
            .await?;
    let mut report = Report { checked: users.len(), ..Default::default() };
    let mut fetched = Vec::with_capacity(users.len());
    for (id, sub, was_allowed) in &users {
        fetched.push((*id, sub.clone(), *was_allowed, fetch_user(app, &token, sub).await?));
    }
    for (id, sub, was_allowed, info) in fetched {
        let (allowed, groups) = match info {
            Some((enabled, groups)) => (enabled && (!app.cfg.provision.needs_phone_group() || groups.contains(&app.cfg.group_phone)), groups),
            None => (false, vec![]),
        };
        sqlx::query("UPDATE users SET phone_allowed = $2, kc_groups = $3 WHERE id = $1")
            .bind(id)
            .bind(allowed)
            .bind(&groups)
            .execute(&app.db)
            .await?;
        if was_allowed && !allowed {
            report.revoked += 1;
            db::audit(&app.db, "system:keycloak-sync", "user.phone_revoked", &format!("user-id:{id}"), serde_json::json!({"sub": sub}))
                .await?;
            for d in db::devices_of(&app.db, id).await? {
                let _ = ami::expire_device(&d.sip_username).await;
                if let Err(e) = ami::hangup_endpoint(&d.sip_username).await {
                    tracing::warn!("hangup {} failed: {e:#}", d.sip_username);
                }
            }
        } else if !was_allowed && allowed {
            report.granted += 1;
            db::audit(&app.db, "system:keycloak-sync", "user.phone_granted", &format!("user-id:{id}"), serde_json::json!({"sub": sub}))
                .await?;
        }
    }
    Ok(report)
}

#[derive(Deserialize)]
struct KcGroupRef {
    id: String,
    name: String,
    #[serde(default, rename = "subGroups")]
    sub_groups: Vec<KcGroupRef>,
}

#[derive(Deserialize)]
struct KcMember {
    id: String,
    username: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "firstName")]
    first_name: Option<String>,
    #[serde(default, rename = "lastName")]
    last_name: Option<String>,
}

/// Enabled members of a Keycloak group (by name, also as a subgroup): (sub, username, e-mail, name).
pub async fn group_members(app: &AppState, token: &str, name: &str) -> Result<Vec<(String, String, Option<String>, String)>> {
    let (cfg, http) = keycloak(app).await?;
    let base = admin_base(&cfg.issuer);
    let resp = http.get(format!("{base}/groups")).query(&[("search", name), ("briefRepresentation", "true")]).bearer_auth(token).send().await?;
    if !resp.status().is_success() {
        bail!("group search: HTTP {} (role query-groups missing?)", resp.status());
    }
    fn find<'a>(gs: &'a [KcGroupRef], name: &str) -> Option<&'a KcGroupRef> {
        gs.iter().find(|g| g.name == name).or_else(|| gs.iter().find_map(|g| find(&g.sub_groups, name)))
    }
    let groups: Vec<KcGroupRef> = resp.json().await?;
    let Some(g) = find(&groups, name) else { bail!("Keycloak group {name:?} not found") };
    let mut out = vec![];
    let mut first = 0;
    loop {
        let resp = http
            .get(format!("{base}/groups/{}/members", g.id))
            .query(&[("first", first.to_string()), ("max", "200".to_string()), ("briefRepresentation", "true".to_string())])
            .bearer_auth(token)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("group members: HTTP {} (role view-users missing?)", resp.status());
        }
        let page: Vec<KcMember> = resp.json().await?;
        let n = page.len();
        for m in page.into_iter().filter(|m| m.enabled) {
            let name = format!("{} {}", m.first_name.unwrap_or_default(), m.last_name.unwrap_or_default()).trim().to_string();
            out.push((m.id, m.username, m.email, name));
        }
        if n < 200 {
            break;
        }
        first += 200;
    }
    Ok(out)
}

#[derive(Deserialize)]
struct KcUserFull {
    id: String,
    username: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "firstName")]
    first_name: Option<String>,
    #[serde(default, rename = "lastName")]
    last_name: Option<String>,
}

/// A Keycloak account found by username (or exact e-mail): (sub, username, email, display name).
pub struct FoundUser {
    pub sub: String,
    pub username: String,
    pub email: Option<String>,
    pub name: String,
}

/// Looks up exactly one Keycloak account by username, or by e-mail if the query contains "@".
/// Linking is always an explicit admin action: the account's `sub` is stored, e-mails are never
/// used to link automatically.
pub async fn find_user(app: &AppState, query: &str) -> Result<FoundUser> {
    let token = token(app).await.context("service account token")?;
    let (cfg, http) = keycloak(app).await?;
    let base = admin_base(&cfg.issuer);
    let key = if query.contains('@') { "email" } else { "username" };
    let resp = http
        .get(format!("{base}/users"))
        .query(&[(key, query), ("exact", "true"), ("max", "2")])
        .bearer_auth(&token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("user search: HTTP {} (role view-users missing?)", resp.status());
    }
    let mut users: Vec<KcUserFull> = resp.json().await?;
    if users.len() != 1 {
        bail!("{} Keycloak accounts match \"{query}\" (exactly one needed)", users.len());
    }
    let u = users.remove(0);
    let name = format!("{} {}", u.first_name.unwrap_or_default(), u.last_name.unwrap_or_default()).trim().to_string();
    Ok(FoundUser { name: if name.is_empty() { u.username.clone() } else { name }, sub: u.id, username: u.username, email: u.email })
}
