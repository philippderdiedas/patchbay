// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-service apps (docs/architecture.md, apps): conference, music, announcement, echo, alias,
//! plus the user's own media uploads used by them.

use std::path::PathBuf;

use askama::Template;
use axum::{
    extract::{Multipart, Path, State},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form as MultiForm;
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use crate::{AppError, AppState, auth::CurrentUser, db, sounds, ui::{Ctx, render}};

pub const KINDS: &[&str] = &["conference", "music", "announcement", "echo", "alias", "clock", "group", "script", "gateway"];

/// Kinds only admins create (organization apps), never personal apps, not even via PBX_USER_APPS:
/// a dial-through bypasses the caller's route permissions.
pub const COMPANY_ONLY_KINDS: &[&str] = &["gateway"];

/// Kinds users get with PBX_USER_APPS=on. A new kind that can reach outside (scripts) is not
/// added here: it has to be listed in PBX_USER_APPS explicitly.
pub const USER_SAFE_KINDS: &[&str] = &["conference", "music", "announcement", "echo", "alias", "clock", "group"];

/// Kinds users may create at this site (PBX_USER_APPS), in the order of KINDS.
pub fn user_kinds(cfg: &crate::config::Config) -> Vec<&'static str> {
    KINDS.iter().copied().filter(|k| cfg.user_app_kinds.iter().any(|u| u == k)).collect()
}

/// English label of an app kind (incl. "custom" for admin dialplan apps), translated in the UI.
/// Used everywhere an app kind is shown: app lists and pages, the help page, CardDAV services.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "conference" => "Conference room",
        "music" => "Hold music",
        "announcement" => "Announcement",
        "echo" => "Echo test",
        "alias" => "Alias",
        "clock" => "Speaking clock",
        "group" => "Call group",
        "script" => "Script",
        "gateway" => "Dial-through",
        "custom" => "Custom app",
        _ => "App",
    }
}

/// What an app of this kind does, in one sentence (call flow diagrams).
pub fn kind_help(kind: &str) -> &'static str {
    match kind {
        "conference" => "Conference room",
        "music" => "Hold music",
        "announcement" => "Plays an announcement",
        "echo" => "Echo test",
        "alias" => "Forwards to another internal number",
        "clock" => "Announces the time every 10 seconds",
        "group" => "Rings the members' phones at the same time",
        "script" => "Runs a script that reads something out (e.g. fetched from the web)",
        "gateway" => "Asks for a PIN and a number, then dials it with a fixed prefix (e.g. another site)",
        _ => "Own dialplan block",
    }
}
const MOH_DIR: &str = "/media/moh";


fn require_phone(app: &AppState, u: &CurrentUser) -> Result<i32, AppError> {
    let uid = u.phone_uid()?;
    if !u.is_admin && !u.may_phone {
        return Err(AppError::Forbidden(format!("No phone permission (group {}).", app.cfg.group_phone)));
    }
    Ok(uid)
}

#[derive(FromRow, Clone)]
pub struct AppRow {
    pub id: i32,
    pub number: i16,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub config: serde_json::Value,
    /// company app (no owner, managed by admins)
    #[sqlx(default)]
    pub company: bool,
}

impl AppRow {
    /// Where to go after saving/deleting
    fn back(&self) -> &'static str {
        if self.company { "/admin/apps" } else { "/" }
    }
    fn cfg(&self, key: &str) -> String {
        match self.config.get(key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            _ => String::new(),
        }
    }
    fn flag(&self, key: &str) -> bool {
        self.config.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    }
}

pub async fn apps_of(db: &PgPool, uid: i32) -> Result<Vec<AppRow>, AppError> {
    Ok(sqlx::query_as("SELECT id, number, name, kind, enabled, config FROM apps WHERE owner_user_id = $1 AND deleted_at IS NULL ORDER BY number")
        .bind(uid)
        .fetch_all(db)
        .await
        .map_err(anyhow::Error::from)?)
}

async fn own_app(db: &PgPool, u: &CurrentUser, id: i32) -> Result<AppRow, AppError> {
    let row: Option<(AppRow, Option<i32>)> = sqlx::query_as::<_, (i32, i16, String, String, bool, serde_json::Value, Option<i32>)>(
        "SELECT id, number, name, kind, enabled, config, owner_user_id FROM apps WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?
    .map(|(id, number, name, kind, enabled, config, owner)| (AppRow { id, number, name, kind, enabled, config, company: owner.is_none() }, owner));
    match row {
        Some((a, owner)) if u.is_admin || (owner.is_some() && u.session.user_id == owner) => Ok(a),
        _ => Err(AppError::NotFound),
    }
}

// --- Create ---------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewApp {
    name: String,
    kind: String,
    /// only with PBX_NUMBERS_APP=…:choose; empty = automatic
    #[serde(default)]
    number: String,
}

pub async fn create(State(app): State<AppState>, u: CurrentUser, axum::Form(f): axum::Form<NewApp>) -> Result<Response, AppError> {
    let uid = require_phone(&app, &u)?;
    if !app.cfg.user_apps {
        return Err(AppError::Forbidden("Personal apps are switched off at this site.".into()));
    }
    if !user_kinds(&app.cfg).contains(&f.kind.as_str()) {
        return Err(AppError::BadRequest("This app type is not available at this site.".into()));
    }
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM apps WHERE owner_user_id = $1 AND deleted_at IS NULL")
        .bind(uid)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if count >= app.cfg.apps_per_user && !u.is_admin {
        return Err(AppError::BadRequest(format!("At most {} apps per user.", app.cfg.apps_per_user)));
    }
    let chosen = if app.cfg.numbers.policy("app").choose() { f.number.as_str() } else { "" };
    let number = crate::numbers::from_form(&app.db, &app.cfg.numbers, "app", chosen, None).await?;
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let config = match f.kind.as_str() {
        "conference" => serde_json::json!({"max_members": 20}),
        "announcement" => serde_json::json!({"repeat": 1}),
        "group" => serde_json::json!({"ring_s": 30}),
        _ => serde_json::json!({}),
    };
    let id: i32 = sqlx::query_scalar("INSERT INTO apps (owner_user_id, number, name, kind, config) VALUES ($1, $2, $3, $4, $5) RETURNING id")
        .bind(uid)
        .bind(number)
        .bind(&name)
        .bind(&f.kind)
        .bind(config)
        .fetch_one(&mut *tx)
        .await
        .map_err(crate::numbers::db_error)?;
    if f.kind == "group" {
        // the owner is the first member (can leave like everybody else)
        sqlx::query("INSERT INTO app_members (app_id, user_id) VALUES ($1, $2)").bind(id).bind(uid).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "app.add", &format!("app:{number}"), serde_json::json!({"kind": f.kind})).await?;
    Ok(Redirect::to(&format!("/apps/{id}")).into_response())
}

// --- Edit -----------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct MediaOption {
    id: i32,
    name: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "app.html")]
struct AppPage {
    c: Ctx,
    a: AppRow,
    media: Vec<MediaOption>,
    /// kind "group": (user id, extension, name)
    members: Vec<(i32, i16, String)>,
}

pub async fn edit(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    let uid = u.session.user_id.unwrap_or(0);
    // Own uploads plus global announcements
    let media = sqlx::query_as(
        "SELECT s.id, s.name, EXISTS (SELECT 1 FROM app_media m WHERE m.app_id = $2 AND m.sound_id = s.id) AS selected
         FROM sounds s WHERE NOT EXISTS (SELECT 1 FROM greetings g WHERE g.sound_id = s.id) AND s.deleted_at IS NULL AND (s.owner_user_id = $1 OR s.owner_user_id IS NULL) ORDER BY s.owner_user_id IS NULL, s.name",
    )
    .bind(uid)
    .bind(id)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let members = if a.kind == "group" { members_of(&app.db, id).await? } else { vec![] };
    render(AppPage { c: Ctx::new(&app, &u), a, media, members })
}

#[derive(Deserialize)]
pub struct AppForm {
    name: String,
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    pin: String,
    #[serde(default)]
    admin_pin: String,
    #[serde(default)]
    max_members: String,
    #[serde(default)]
    repeat: String,
    #[serde(default)]
    shuffle: Option<String>,
    #[serde(default)]
    target: String,
    #[serde(default)]
    media: Vec<i32>,
    #[serde(default)]
    ring_s: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    source: String,
}

fn pin_ok(p: &str) -> bool {
    p.is_empty() || (p.len() >= 3 && p.len() <= 8 && p.chars().all(|c| c.is_ascii_digit()))
}

/// Rebuilds the MOH directory of an app: hard links to the converted media in playlist order.
async fn sync_moh(app_id: i32, media: &[i32]) -> anyhow::Result<()> {
    let dir = PathBuf::from(format!("{MOH_DIR}/app{app_id}"));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    if media.is_empty() {
        return Ok(());
    }
    tokio::fs::create_dir_all(&dir).await?;
    for (pos, sid) in media.iter().enumerate() {
        for ext in ["sln16", "alaw"] {
            let src = format!("{}/asterisk/{sid}.{ext}", sounds::MEDIA_DIR);
            let dst = dir.join(format!("{:03}-{sid}.{ext}", pos + 1));
            if tokio::fs::hard_link(&src, &dst).await.is_err() {
                tokio::fs::copy(&src, &dst).await?;
            }
        }
    }
    Ok(())
}

/// Personal apps only of the kinds this site allows users (PBX_USER_APPS), like `create`: a kind
/// removed from the list later can no longer be changed or tested (the dialplan ignores it anyway).
fn require_user_kind(app: &AppState, a: &AppRow) -> Result<(), AppError> {
    if !a.company && !user_kinds(&app.cfg).contains(&a.kind.as_str()) {
        return Err(AppError::BadRequest("This app type is not available at this site.".into()));
    }
    Ok(())
}

pub async fn save(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, MultiForm(f): MultiForm<AppForm>) -> Result<Response, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    require_user_kind(&app, &a)?;
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let mut cfg = serde_json::Map::new();
    match a.kind.as_str() {
        "conference" => {
            if !pin_ok(&f.pin) || !pin_ok(&f.admin_pin) {
                return Err(AppError::BadRequest("PINs: 3–8 digits or empty.".into()));
            }
            let max: i64 = f.max_members.parse().unwrap_or(20).clamp(2, 50);
            cfg.insert("pin".into(), f.pin.clone().into());
            cfg.insert("admin_pin".into(), f.admin_pin.clone().into());
            cfg.insert("max_members".into(), max.into());
        }
        "music" => {
            cfg.insert("shuffle".into(), f.shuffle.is_some().into());
        }
        "announcement" => {
            cfg.insert("repeat".into(), f.repeat.parse::<i64>().unwrap_or(1).clamp(1, 10).into());
        }
        "alias" => {
            // Internal numbers only (persons, call groups, devices, apps, IVRs/custom apps)
            let t = f.target.trim();
            if t.len() != 4 || !t.chars().all(|c| c.is_ascii_digit()) || t == a.number.to_string() || t.starts_with('0') {
                return Err(AppError::BadRequest("Alias target: an internal number with 4 digits.".into()));
            }
            let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM numbers WHERE number::text = $1)")
                .bind(t)
                .fetch_one(&app.db)
                .await
                .map_err(anyhow::Error::from)?;
            if !exists {
                return Err(AppError::BadRequest(format!("Alias target: {t} is not assigned.")));
            }
            // no chains: the target must not be an alias itself
            let chained: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM apps WHERE number::text = $1 AND kind = 'alias' AND deleted_at IS NULL)")
                .bind(t)
                .fetch_one(&app.db)
                .await
                .map_err(anyhow::Error::from)?;
            if chained {
                return Err(AppError::BadRequest("Alias target is an alias itself.".into()));
            }
            cfg.insert("target".into(), t.into());
        }
        "group" => {
            let prefix = f.prefix.trim();
            if !crate::admin::valid_caller_prefix(prefix) {
                return Err(AppError::BadRequest("Caller name prefix: at most 8 characters, no , ; : \" \\ | < >".into()));
            }
            cfg.insert("ring_s".into(), f.ring_s.parse::<i64>().unwrap_or(30).clamp(10, 120).into());
            cfg.insert("prefix".into(), prefix.into());
        }
        "script" => {
            let src = f.source.replace("\r\n", "\n");
            if src.len() > crate::script::MAX_SOURCE {
                return Err(AppError::BadRequest("Script too long (20 KB).".into()));
            }
            crate::script::check(&src).map_err(|e| AppError::BadRequest(format!("Script: {e}")))?;
            cfg.insert("source".into(), src.into());
        }
        "gateway" => {
            if !a.company || !u.is_admin {
                return Err(AppError::Forbidden("Dial-throughs are organization apps.".into()));
            }
            if f.pin.is_empty() || !pin_ok(&f.pin) {
                return Err(AppError::BadRequest("PIN: 3–8 digits (required).".into()));
            }
            let t = f.target.trim();
            if t.is_empty() || t.len() > 8 || !t.chars().all(|c| c.is_ascii_digit()) {
                return Err(AppError::BadRequest("Prefix: 1–8 digits (e.g. 78 for a site).".into()));
            }
            // the prefix must lead to a dial rule, otherwise every call ends in "invalid"
            let routed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM outbound_routes WHERE action = 'route' AND $1 LIKE match_prefix || '%')")
                .bind(t)
                .fetch_one(&app.db)
                .await
                .map_err(anyhow::Error::from)?;
            if !routed {
                return Err(AppError::BadRequest(format!("Prefix {t}: no dial rule routes it.")));
            }
            cfg.insert("pin".into(), f.pin.clone().into());
            cfg.insert("target".into(), t.into());
        }
        _ => {}
    }
    // Media: only own uploads or global announcements; announcement uses the first selected one
    let uid = u.session.user_id.unwrap_or(0);
    let allowed: Vec<i32> = sqlx::query_scalar("SELECT id FROM sounds WHERE deleted_at IS NULL AND (owner_user_id = $1 OR owner_user_id IS NULL)")
        .bind(uid)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let mut media: Vec<i32> = f.media.iter().copied().filter(|m| allowed.contains(m) || u.is_admin).collect();
    // keep the first occurrence of each id (order matters for playlists), drop later duplicates
    let mut seen = std::collections::HashSet::new();
    media.retain(|m| seen.insert(*m));
    if a.kind == "announcement" {
        media.truncate(1);
    }
    if !matches!(a.kind.as_str(), "conference" | "music" | "announcement") {
        media.clear();
    }

    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE apps SET name = $2, enabled = $3, config = $4 WHERE id = $1")
        .bind(id)
        .bind(&name)
        .bind(f.enabled.is_some())
        .bind(serde_json::Value::Object(cfg))
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM app_media WHERE app_id = $1").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    for (pos, sid) in media.iter().enumerate() {
        sqlx::query("INSERT INTO app_media (app_id, sound_id, position) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(sid)
            .bind(pos as i16)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    sync_moh(id, &media).await.map_err(|e| AppError::Internal(e.context("music on hold")))?;
    db::audit(&app.db, &u.session.sub, "app.edit", &format!("app:{}", a.number), serde_json::json!({"media": media.len()})).await?;
    Ok(Redirect::to(a.back()).into_response())
}

pub async fn delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    // numbers, rules and forwards that lead to the app would end in "service not available"
    let refs: Vec<String> = sqlx::query_scalar(&crate::admin::target_refs("SELECT id FROM targets WHERE app_id = $2"))
        .bind(0)
        .bind(id)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if !refs.is_empty() {
        return Err(AppError::BadRequest(format!("App {} is still a call target: {}. Change these first.", a.number, refs.join(", "))));
    }
    sqlx::query("UPDATE apps SET deleted_at = now(), enabled = false WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    let _ = sync_moh(id, &[]).await;
    db::audit(&app.db, &u.session.sub, "app.delete", &format!("app:{}", a.number), serde_json::json!({})).await?;
    Ok(Redirect::to(a.back()).into_response())
}

// --- My media ------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct MyMedia {
    id: i32,
    name: String,
    duration_s: Option<f64>,
    used: i64,
}

#[derive(Template)]
#[template(path = "media.html")]
struct MediaPage {
    c: Ctx,
    media: Vec<MyMedia>,
    used_mb: f64,
    quota_mb: i64,
    user_moh: bool,
    hold_shuffle: bool,
    hold_tracks: Vec<crate::moh::TrackChoice>,
}

async fn used_bytes(uid: i32, db: &PgPool) -> i64 {
    let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM sounds WHERE owner_user_id = $1 AND deleted_at IS NULL")
        .bind(uid)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut total = 0;
    for id in ids {
        for ext in ["sln16", "alaw"] {
            if let Ok(m) = tokio::fs::metadata(format!("{}/asterisk/{id}.{ext}", sounds::MEDIA_DIR)).await {
                total += m.len() as i64;
            }
        }
    }
    total
}

pub async fn media(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = require_phone(&app, &u)?;
    let media = sqlx::query_as(
        "SELECT s.id, s.name, s.duration_s::float8 AS duration_s,
                (SELECT count(*) FROM app_media m WHERE m.sound_id = s.id) + (SELECT count(*) FROM moh_media m WHERE m.sound_id = s.id) AS used
         FROM sounds s WHERE NOT EXISTS (SELECT 1 FROM greetings g WHERE g.sound_id = s.id) AND s.owner_user_id = $1 AND s.deleted_at IS NULL ORDER BY s.name",
    )
    .bind(uid)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let used_mb = used_bytes(uid, &app.db).await as f64 / 1_048_576.0;
    let (hold_shuffle, hold_tracks) = if app.cfg.user_moh { crate::moh::personal(&app.db, uid).await? } else { (true, vec![]) };
    render(MediaPage { c: Ctx::new(&app, &u), media, used_mb, quota_mb: app.cfg.media_quota_mb, user_moh: app.cfg.user_moh, hold_shuffle, hold_tracks })
}

pub async fn media_upload(State(app): State<AppState>, u: CurrentUser, mut mp: Multipart) -> Result<Response, AppError> {
    let uid = require_phone(&app, &u)?;
    if used_bytes(uid, &app.db).await >= app.cfg.media_quota_mb * 1_048_576 {
        return Err(AppError::BadRequest("Storage quota reached.".into()));
    }
    let mut name = String::new();
    let mut file: Option<(String, axum::body::Bytes)> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        match field.name() {
            Some("name") => name = field.text().await.unwrap_or_default().trim().to_string(),
            Some("file") => {
                let fname = field.file_name().unwrap_or("upload").to_string();
                file = Some((fname, field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?));
            }
            _ => {}
        }
    }
    let (fname, data) = file.filter(|(_, d)| !d.is_empty()).ok_or_else(|| AppError::BadRequest("No file.".into()))?;
    if name.is_empty() {
        name = fname.rsplit_once('.').map(|(n, _)| n.to_string()).unwrap_or(fname.clone());
    }
    let name: String = name.chars().take(60).collect();
    let id: i32 = sqlx::query_scalar("INSERT INTO sounds (name, language, file_path, source_name, owner_user_id) VALUES ($1, 'de', '', $2, $3) RETURNING id")
        .bind(&name)
        .bind(&fname)
        .bind(uid)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let tmp = std::env::temp_dir().join(format!("upload-{id}"));
    tokio::fs::write(&tmp, &data).await.map_err(anyhow::Error::from)?;
    let res = sounds::convert(&tmp, id).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    match res {
        Ok(dur) => {
            sqlx::query("UPDATE sounds SET file_path = id::text, duration_s = $2 WHERE id = $1")
                .bind(id)
                .bind(dur)
                .execute(&app.db)
                .await
                .map_err(anyhow::Error::from)?;
            db::audit(&app.db, &u.session.sub, "media.add", &format!("sound:{id}"), serde_json::json!({"name": name})).await?;
            Ok(Redirect::to("/media").into_response())
        }
        Err(e) => {
            sqlx::query("DELETE FROM sounds WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
            Err(AppError::BadRequest(format!("The file could not be converted: {e}")))
        }
    }
}

pub async fn media_delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = require_phone(&app, &u)?;
    let n = sqlx::query(
        "UPDATE sounds s SET deleted_at = now() WHERE id = $1 AND owner_user_id = $2 AND deleted_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM app_media m WHERE m.sound_id = s.id)
           AND NOT EXISTS (SELECT 1 FROM moh_media m WHERE m.sound_id = s.id)",
    )
    .bind(id)
    .bind(uid)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("The file is still used by an app or your hold music.".into()));
    }
    for f in [format!("asterisk/{id}.sln16"), format!("asterisk/{id}.alaw"), format!("preview/{id}.ogg")] {
        let _ = tokio::fs::remove_file(format!("{}/{f}", sounds::MEDIA_DIR)).await;
    }
    db::audit(&app.db, &u.session.sub, "media.delete", &format!("sound:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/media").into_response())
}

/// Preview of own media (admins: any).
pub async fn media_preview(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.session.user_id.unwrap_or(0);
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sounds WHERE id = $1 AND deleted_at IS NULL AND (owner_user_id = $2 OR owner_user_id IS NULL OR $3))")
        .bind(id)
        .bind(uid)
        .bind(u.is_admin)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if !ok {
        return Err(AppError::NotFound);
    }
    let data = tokio::fs::read(format!("{}/preview/{id}.ogg", sounds::MEDIA_DIR)).await.map_err(|_| AppError::NotFound)?;
    Ok(([(axum::http::header::CONTENT_TYPE, "audio/ogg")], axum::body::Body::from(data)).into_response())
}

// --- Company apps (admin) ------------------------------------------------------------------

pub async fn company_apps(db: &PgPool) -> Result<Vec<AppRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, number, name, kind, enabled, config, true AS company FROM apps
         WHERE owner_user_id IS NULL AND deleted_at IS NULL ORDER BY number",
    )
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

#[derive(Deserialize)]
pub struct NewCompanyApp {
    #[serde(default)]
    number: String,
    name: String,
    kind: String,
}

/// Admins create company apps (number from the company range; empty = next free).
pub async fn company_create(State(app): State<AppState>, crate::auth::Admin(u): crate::auth::Admin, axum::Form(f): axum::Form<NewCompanyApp>) -> Result<Response, AppError> {
    if !KINDS.contains(&f.kind.as_str()) {
        return Err(AppError::BadRequest("Unknown app type.".into()));
    }
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let number = crate::numbers::from_form(&app.db, &app.cfg.numbers, "company_app", &f.number, None).await?;
    let config = match f.kind.as_str() {
        "conference" => serde_json::json!({"max_members": 50}),
        "announcement" => serde_json::json!({"repeat": 1}),
        "group" => serde_json::json!({"ring_s": 30}),
        _ => serde_json::json!({}),
    };
    let id: i32 = sqlx::query_scalar("INSERT INTO apps (owner_user_id, number, name, kind, config) VALUES (NULL, $1, $2, $3, $4) RETURNING id")
        .bind(number)
        .bind(&name)
        .bind(&f.kind)
        .bind(config)
        .fetch_one(&app.db)
        .await
        .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &u.session.sub, "app.add", &format!("app:{number}"), serde_json::json!({"kind": f.kind, "company": true})).await?;
    Ok(Redirect::to(&format!("/apps/{id}")).into_response())
}

// --- Call group apps: members ----------------------------------------------------------------

async fn members_of(db: &PgPool, app_id: i32) -> Result<Vec<(i32, i16, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.extension, u.display_name FROM app_members m JOIN users u ON u.id = m.user_id
         WHERE m.app_id = $1 AND u.deleted_at IS NULL ORDER BY m.added_at, u.extension",
    )
    .bind(app_id)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

#[derive(Deserialize)]
pub struct MemberForm {
    extension: String,
}

/// POST /apps/{id}/members: the owner (or an admin) adds a person by extension.
pub async fn member_add(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, axum::Form(f): axum::Form<MemberForm>) -> Result<Response, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    if a.kind != "group" {
        return Err(AppError::NotFound);
    }
    let ext = f.extension.trim();
    let uid: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE extension::text = $1 AND enabled AND deleted_at IS NULL")
        .bind(ext)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let Some(uid) = uid else { return Err(AppError::BadRequest(format!("No active person with extension {ext}."))) };
    sqlx::query("INSERT INTO app_members (app_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(uid)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "app.member_add", &format!("app:{}", a.number), serde_json::json!({"extension": ext})).await?;
    Ok(Redirect::to(&format!("/apps/{id}#members")).into_response())
}

/// POST /apps/{id}/members/{uid}/delete: the owner (or an admin) removes a member.
pub async fn member_remove(State(app): State<AppState>, u: CurrentUser, Path((id, uid)): Path<(i32, i32)>) -> Result<Response, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    sqlx::query("DELETE FROM app_members WHERE app_id = $1 AND user_id = $2").bind(id).bind(uid).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "app.member_remove", &format!("app:{}", a.number), serde_json::json!({"user_id": uid})).await?;
    Ok(Redirect::to(&format!("/apps/{id}#members")).into_response())
}

/// Call group apps the person is a member of (start page): (app id, number, name, owner name).
pub async fn memberships(db: &PgPool, uid: i32) -> Result<Vec<(i32, i16, String, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT a.id, a.number, a.name, COALESCE(o.display_name, '') FROM app_members m JOIN apps a ON a.id = m.app_id
         LEFT JOIN users o ON o.id = a.owner_user_id
         WHERE m.user_id = $1 AND a.deleted_at IS NULL AND a.kind = 'group' ORDER BY a.number",
    )
    .bind(uid)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// POST /me/app-groups/{id}/leave: a member leaves a call group app.
pub async fn leave(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let n = sqlx::query("DELETE FROM app_members WHERE app_id = $1 AND user_id = $2")
        .bind(id)
        .bind(uid)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if n > 0 {
        db::audit(&app.db, &u.session.sub, "app.member_leave", &format!("app-id:{id}"), serde_json::json!({})).await?;
    }
    Ok(Redirect::to("/#groups").into_response())
}

// --- Script apps -------------------------------------------------------------------------------

/// Runs the script of an app and speaks its output: Playback paths, or an error message.
async fn run_script(app: &AppState, a: &AppRow, caller: &str, lang: &str) -> Result<(Vec<String>, crate::script::Output), String> {
    let Some(tts) = app.cfg.tts.clone() else { return Err("Text to speech is not configured (PBX_TTS).".into()) };
    let src = a.cfg("source");
    let out = match crate::script::run(&src, caller, lang, app.cfg.script_http_allow.clone()).await {
        Ok(o) => o,
        Err((msg, _)) => return Err(msg),
    };
    let mut files = vec![];
    for (l, text) in &out.says {
        files.push(crate::tts::speak(&tts, l, text).await.map_err(|e| format!("TTS: {e:#}"))?);
    }
    Ok((files, out))
}

#[derive(serde::Serialize)]
pub struct TestResult {
    ok: bool,
    says: Vec<String>,
    log: Vec<String>,
    error: String,
}

/// POST /apps/{id}/test: runs the saved script as if called by the owner, shows text and log.
pub async fn script_test(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<axum::Json<TestResult>, AppError> {
    let a = own_app(&app.db, &u, id).await?;
    if a.kind != "script" {
        return Err(AppError::NotFound);
    }
    require_user_kind(&app, &a)?;
    let caller = match u.session.user_id {
        Some(uid) => db::user_extension(&app.db, uid).await.map(|e| e.to_string()).unwrap_or_default(),
        None => String::new(),
    };
    let lang = u.lang.code();
    let res = match crate::script::run(&a.cfg("source"), &caller, lang, app.cfg.script_http_allow.clone()).await {
        Ok(o) => TestResult { ok: true, says: o.says.into_iter().map(|(_, t)| t).collect(), log: o.log, error: String::new() },
        Err((msg, o)) => TestResult { ok: false, says: o.says.into_iter().map(|(_, t)| t).collect(), log: o.log, error: msg },
    };
    db::audit(&app.db, &u.session.sub, "app.script_test", &format!("app:{}", a.number), serde_json::json!({"ok": res.ok})).await?;
    Ok(axum::Json(res))
}

#[derive(Deserialize)]
struct ScriptCall {
    id: i32,
    #[serde(default)]
    caller: String,
    #[serde(default)]
    lang: String,
}

/// POST /internal/app/script (dialplan sub-app exten script): "ok,<file>,<file>…" or "err".
async fn internal_script(State(app): State<AppState>, axum::Form(f): axum::Form<ScriptCall>) -> String {
    let row: Option<(i32, i16, String, String, bool, serde_json::Value)> =
        sqlx::query_as("SELECT a.id, a.number, a.name, a.kind, a.enabled, a.config FROM apps a WHERE a.id = $1 AND a.kind = 'script' AND a.deleted_at IS NULL AND a.id IN (SELECT id FROM app_dialplan)")
            .bind(f.id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
    let Some((id, number, name, kind, enabled, config)) = row else { return "err".into() };
    let a = AppRow { id, number, name, kind, enabled, config, company: false };
    let caller: String = f.caller.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    match run_script(&app, &a, &caller, &f.lang).await {
        Ok((files, _)) => std::iter::once("ok".to_string()).chain(files).collect::<Vec<_>>().join(","),
        Err(e) => {
            tracing::info!("script app {number}: {e}");
            "err".into()
        }
    }
}

#[derive(Deserialize)]
struct PinFail {
    id: i32,
    /// "gateway" (dial-through PIN), "conference" (room PIN), "moderator" (moderator PIN via *0)
    what: String,
    #[serde(default)]
    caller: String,
    /// failures counted in the current 15-minute window (empty while locked)
    #[serde(default)]
    count: String,
    /// "1": the app's PIN is locked (just now or still)
    #[serde(default)]
    locked: String,
}

/// POST /internal/app/pinfail (dialplan sub-pinfail): a wrong app PIN. Counting and the lock live in
/// the Asterisk database (they work without patchbay-web); this only writes the audit log, which /monitor
/// reads (`monitor.rs`, service "PBX App PINs").
async fn internal_pinfail(State(app): State<AppState>, axum::Form(f): axum::Form<PinFail>) -> &'static str {
    let number: Option<i16> = sqlx::query_scalar("SELECT number FROM apps WHERE id = $1").bind(f.id).fetch_optional(&app.db).await.ok().flatten();
    let caller: String = f.caller.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    let what: String = f.what.chars().filter(|c| c.is_ascii_lowercase()).take(20).collect();
    let locked = f.locked == "1";
    let object = number.map(|n| format!("app:{n}")).unwrap_or_else(|| format!("app-id:{}", f.id));
    let details = serde_json::json!({"what": what, "caller": caller, "count": f.count.parse::<i64>().ok(), "locked": locked});
    match db::audit(&app.db, "system:asterisk", "app.pin_failed", &object, details).await {
        Ok(()) => "ok",
        Err(e) => {
            tracing::warn!("audit of a wrong PIN for {object} failed: {e:#}");
            "err"
        }
    }
}

pub fn internal_router(app: AppState) -> axum::Router {
    axum::Router::new()
        .route("/internal/app/script", axum::routing::post(internal_script))
        .route("/internal/app/pinfail", axum::routing::post(internal_pinfail))
        .with_state(app)
}
