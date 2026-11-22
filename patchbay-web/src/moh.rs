// SPDX-License-Identifier: GPL-3.0-or-later
//! Hold music playlists : company playlists (one is the default hold music, others
//! can be chosen per group) and one personal playlist per user (PBX_USER_MOH), all built from the
//! audio library. patchbay-web hard-links the converted files into /media/moh/<class>; Asterisk reads
//! the default class from musiconhold.conf and the others via realtime.

use std::path::PathBuf;

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form as MultiForm;
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use tokio::process::Command;

use crate::{AppError, AppState, auth::Admin, auth::CurrentUser, db, sounds, ui::{Ctx, render}};

pub const MOH_DIR: &str = "/media/moh";
/// Initial hold music of the site, imported once: a site's own files in /media/moh-default, else
/// the Opsound music shipped in the image (patchbay-web/Dockerfile).
const SEED_DIR: &str = "/media/moh-default";
const SEED_DIR_IMAGE: &str = "/usr/share/pbx/moh-default";


// --- Files -----------------------------------------------------------------------------------

/// Replaces /media/moh/<name> with hard links to the media in playlist order. The new directory is
/// built next to the old one and swapped in, so a caller on hold never finds it empty.
async fn sync_dir(name: &str, media: &[i32]) -> anyhow::Result<()> {
    let dir = PathBuf::from(MOH_DIR).join(name);
    if media.is_empty() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Ok(());
    }
    let new = PathBuf::from(MOH_DIR).join(format!(".{name}.new"));
    let old = PathBuf::from(MOH_DIR).join(format!(".{name}.old"));
    let _ = tokio::fs::remove_dir_all(&new).await;
    tokio::fs::create_dir_all(&new).await?;
    for (pos, sid) in media.iter().enumerate() {
        for ext in ["sln16", "alaw"] {
            let src = format!("{}/asterisk/{sid}.{ext}", sounds::MEDIA_DIR);
            let dst = new.join(format!("{:03}-{sid}.{ext}", pos + 1));
            if tokio::fs::hard_link(&src, &dst).await.is_err() {
                tokio::fs::copy(&src, &dst).await?;
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&old).await;
    if tokio::fs::metadata(&dir).await.is_ok() {
        tokio::fs::rename(&dir, &old).await?;
    }
    tokio::fs::rename(&new, &dir).await?;
    let _ = tokio::fs::remove_dir_all(&old).await;
    Ok(())
}

async fn media_of(db: &PgPool, id: i32) -> anyhow::Result<Vec<i32>> {
    Ok(sqlx::query_scalar(
        "SELECT m.sound_id FROM moh_media m JOIN sounds s ON s.id = m.sound_id AND s.deleted_at IS NULL
         WHERE m.playlist_id = $1 ORDER BY m.position, m.sound_id",
)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Writes the files of one playlist; the default playlist goes to the "default" class (reloaded
/// in Asterisk), an empty default playlist leaves the previous files in place.
pub async fn sync_playlist(db: &PgPool, id: i32) -> anyhow::Result<()> {
    let Some(is_default): Option<bool> = sqlx::query_scalar("SELECT is_default FROM moh_playlists WHERE id = $1").bind(id).fetch_optional(db).await?
    else {
        return sync_dir(&format!("pl{id}"), &[]).await;
    };
    let media = media_of(db, id).await?;
    if is_default {
        if !media.is_empty() {
            sync_dir("default", &media).await?;
            if let Err(e) = crate::ami::command("moh reload").await {
                tracing::warn!("moh reload: {e:#}");
            }
        }
    } else {
        sync_dir(&format!("pl{id}"), &media).await?;
    }
    Ok(())
}

/// Startup: publish the personal-hold-music switch for the endpoint view, import the initial hold
/// music once, and rebuild all playlist directories.
pub async fn startup(app: &AppState) {
    let flag = if app.cfg.user_moh { "true" } else { "false" };
    if let Err(e) = sqlx::query("INSERT INTO settings (key, value) VALUES ('user_moh', $1) ON CONFLICT (key) DO UPDATE SET value = $1")
        .bind(flag)
        .execute(&app.db)
        .await
    {
        tracing::warn!("settings user_moh: {e:#}");
    }
    if let Err(e) = import_seed(&app.db).await {
        tracing::warn!("hold music import: {e:#}");
    }
    let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM moh_playlists").fetch_all(&app.db).await.unwrap_or_default();
    for id in ids {
        if let Err(e) = sync_playlist(&app.db, id).await {
            tracing::warn!("hold music playlist {id}: {e:#}");
        }
    }
}

/// Imports the files of /media/moh-default into the library as the company hold music, once
/// (when no default playlist exists). Files are Asterisk formats: .sln16 (raw 16 kHz) or .wav.
async fn import_seed(db: &PgPool) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM moh_playlists WHERE is_default)").fetch_one(db).await?;
    if exists {
        return Ok(());
    }
    let mut stems = seed_files(SEED_DIR).await?;
    if stems.is_empty() {
        stems = seed_files(SEED_DIR_IMAGE).await?;
    }
    if stems.is_empty() {
        return Ok(());
    }
    import_files(db, stems).await
}

/// Audio files of a seed directory by name (.sln16 preferred over .wav); missing directory: none.
async fn seed_files(dir: &str) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
    let mut stems: std::collections::BTreeMap<String, PathBuf> = Default::default();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return Ok(stems),
    };
    while let Some(e) = rd.next_entry().await? {
        let p = e.path();
        let (Some(stem), Some(ext)) = (p.file_stem().and_then(|s| s.to_str()), p.extension().and_then(|s| s.to_str())) else { continue };
        match ext {
            "sln16" => {
                stems.insert(stem.to_string(), p.clone());
            }
            "wav" => {
                stems.entry(stem.to_string()).or_insert(p.clone());
            }
            _ => {}
        }
    }
    Ok(stems)
}

async fn import_files(db: &PgPool, stems: std::collections::BTreeMap<String, PathBuf>) -> anyhow::Result<()> {
    let mut ids = vec![];
    for (stem, path) in stems {
        let id: i32 = sqlx::query_scalar("INSERT INTO sounds (name, language, file_path, source_name) VALUES ($1, 'de', '', $2) RETURNING id")
            .bind(&stem)
            .bind(path.file_name().and_then(|f| f.to_str()).unwrap_or(""))
            .fetch_one(db)
            .await?;
        // trusted local file: decode raw sln16 to WAV first, then the normal conversion
        let wav = std::env::temp_dir().join(format!("seed-{id}.wav"));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-v", "error"]);
        if path.extension().and_then(|e| e.to_str()) == Some("sln16") {
            cmd.args(["-f", "s16le", "-ar", "16000", "-ac", "1"]);
        }
        cmd.arg("-i").arg(&path).arg(&wav);
        let res = match sounds::run(&mut cmd).await {
            Ok(_) => sounds::convert(&wav, id).await,
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_file(&wav).await;
        match res {
            Ok(dur) => {
                sqlx::query("UPDATE sounds SET file_path = id::text, duration_s = $2 WHERE id = $1").bind(id).bind(dur).execute(db).await?;
                ids.push(id);
            }
            Err(e) => {
                tracing::warn!("hold music import {}: {e:#}", path.display());
                sqlx::query("DELETE FROM sounds WHERE id = $1").bind(id).execute(db).await?;
            }
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    let pid: i32 = sqlx::query_scalar("INSERT INTO moh_playlists (name, is_default) VALUES ('Standard', true) RETURNING id").fetch_one(db).await?;
    for (pos, sid) in ids.iter().enumerate() {
        sqlx::query("INSERT INTO moh_media (playlist_id, sound_id, position) VALUES ($1, $2, $3)").bind(pid).bind(sid).bind(pos as i16).execute(db).await?;
    }
    tracing::info!("hold music: imported {} files as the company hold music", ids.len());
    Ok(())
}

// --- Data for pages ----------------------------------------------------------------------------

#[derive(FromRow)]
pub struct PlaylistRow {
    pub id: i32,
    pub name: String,
    pub is_default: bool,
    pub shuffle: bool,
    pub tracks: i64,
    pub groups: i64,
}

pub async fn company_playlists(db: &PgPool) -> Result<Vec<PlaylistRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT p.id, p.name, p.is_default, p.shuffle,
                (SELECT count(*) FROM moh_media m JOIN sounds s ON s.id = m.sound_id AND s.deleted_at IS NULL WHERE m.playlist_id = p.id) AS tracks,
                (SELECT count(*) FROM groups g WHERE g.moh_playlist_id = p.id AND g.deleted_at IS NULL) AS groups
         FROM moh_playlists p WHERE p.owner_user_id IS NULL ORDER BY NOT p.is_default, lower(p.name)",
)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// A library entry with its selection state in a playlist.
#[derive(FromRow)]
pub struct TrackChoice {
    pub id: i32,
    pub name: String,
    pub duration_s: Option<f64>,
    pub selected: bool,
}

/// Library entries (company: owner NULL; personal: owner = uid, without voicemail greetings).
async fn choices(db: &PgPool, playlist: Option<i32>, owner: Option<i32>) -> Result<Vec<TrackChoice>, AppError> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.name, s.duration_s::float8 AS duration_s,
                EXISTS (SELECT 1 FROM moh_media m WHERE m.playlist_id = $1 AND m.sound_id = s.id) AS selected
         FROM sounds s
         WHERE s.deleted_at IS NULL AND s.file_path <> '' AND s.owner_user_id IS NOT DISTINCT FROM $2
           AND NOT EXISTS (SELECT 1 FROM greetings g WHERE g.sound_id = s.id)
         ORDER BY lower(s.name)",
)
    .bind(playlist.unwrap_or(0))
    .bind(owner)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// Replaces the tracks of a playlist with the given library entries (only ones the owner may use).
async fn set_tracks(db: &PgPool, id: i32, owner: Option<i32>, media: &[i32]) -> Result<(), AppError> {
    let mut tx = db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM moh_media WHERE playlist_id = $1").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    let mut seen = std::collections::HashSet::new();
    for (pos, sid) in media.iter().filter(|s| seen.insert(**s)).enumerate() {
        sqlx::query(
            "INSERT INTO moh_media (playlist_id, sound_id, position)
             SELECT $1, s.id, $3 FROM sounds s WHERE s.id = $2 AND s.deleted_at IS NULL AND s.file_path <> ''
               AND s.owner_user_id IS NOT DISTINCT FROM $4",
)
        .bind(id)
        .bind(sid)
        .bind(pos as i16)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}

// --- Admin: company playlists ----------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin_moh.html")]
struct PlaylistPage {
    c: Ctx,
    p: PlaylistRow,
    tracks: Vec<TrackChoice>,
}

#[derive(Deserialize)]
pub struct NewPlaylist {
    name: String,
}

pub async fn create(State(app): State<AppState>, Admin(a): Admin, axum::Form(f): axum::Form<NewPlaylist>) -> Result<Response, AppError> {
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let id: i32 = sqlx::query_scalar("INSERT INTO moh_playlists (name) VALUES ($1) RETURNING id")
        .bind(&name)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "moh.create", &format!("moh:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to(&format!("/admin/moh/{id}")).into_response())
}

async fn company_playlist(db: &PgPool, id: i32) -> Result<PlaylistRow, AppError> {
    company_playlists(db).await?.into_iter().find(|p| p.id == id).ok_or(AppError::NotFound)
}

pub async fn edit(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p = company_playlist(&app.db, id).await?;
    let tracks = choices(&app.db, Some(id), None).await?;
    render(PlaylistPage { c: Ctx::new(&app, &a).section("sounds"), p, tracks })
}

#[derive(Deserialize)]
pub struct PlaylistForm {
    name: String,
    #[serde(default)]
    shuffle: Option<String>,
    #[serde(default)]
    media: Vec<i32>,
}

pub async fn save(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, MultiForm(f): MultiForm<PlaylistForm>) -> Result<Response, AppError> {
    let p = company_playlist(&app.db, id).await?;
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    if p.is_default && f.media.is_empty() {
        return Err(AppError::BadRequest("The company hold music needs at least one track.".into()));
    }
    sqlx::query("UPDATE moh_playlists SET name = $2, shuffle = $3 WHERE id = $1")
        .bind(id)
        .bind(&name)
        .bind(f.shuffle.is_some())
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    set_tracks(&app.db, id, None, &f.media).await?;
    sync_playlist(&app.db, id).await.map_err(|e| AppError::BadRequest(format!("Hold music could not be written: {e}")))?;
    db::audit(&app.db, &a.session.sub, "moh.save", &format!("moh:{id}"), serde_json::json!({"name": name, "tracks": f.media})).await?;
    Ok(Redirect::to("/admin/sounds#moh").into_response())
}

/// Makes a company playlist the default hold music.
pub async fn make_default(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p = company_playlist(&app.db, id).await?;
    if p.tracks == 0 {
        return Err(AppError::BadRequest("The company hold music needs at least one track.".into()));
    }
    let old: Option<i32> = sqlx::query_scalar("SELECT id FROM moh_playlists WHERE is_default").fetch_optional(&app.db).await.map_err(anyhow::Error::from)?;
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE moh_playlists SET is_default = false WHERE is_default").execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE moh_playlists SET is_default = true WHERE id = $1").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    // groups that chose this playlist explicitly now simply use the default
    sqlx::query("UPDATE groups SET moh_playlist_id = NULL WHERE moh_playlist_id = $1").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    sync_playlist(&app.db, id).await.map_err(|e| AppError::BadRequest(format!("Hold music could not be written: {e}")))?;
    if let Some(o) = old {
        let _ = sync_playlist(&app.db, o).await; // becomes a normal class pl<id>
    }
    let _ = sync_dir(&format!("pl{id}"), &[]).await;
    db::audit(&app.db, &a.session.sub, "moh.default", &format!("moh:{id}"), serde_json::json!({"previous": old})).await?;
    Ok(Redirect::to("/admin/sounds#moh").into_response())
}

pub async fn delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p = company_playlist(&app.db, id).await?;
    if p.is_default {
        return Err(AppError::BadRequest("The company hold music cannot be deleted. Make another playlist the default first.".into()));
    }
    if p.groups > 0 {
        return Err(AppError::BadRequest("The playlist is still used by a group.".into()));
    }
    sqlx::query("DELETE FROM moh_playlists WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    let _ = sync_dir(&format!("pl{id}"), &[]).await;
    db::audit(&app.db, &a.session.sub, "moh.delete", &format!("moh:{id}"), serde_json::json!({"name": p.name})).await?;
    Ok(Redirect::to("/admin/sounds#moh").into_response())
}

// --- Users: personal hold music ----------------------------------------------------------------

/// The user's personal hold music: (playlist id, shuffle) and the selectable own media.
pub async fn personal(db: &PgPool, uid: i32) -> Result<(bool, Vec<TrackChoice>), AppError> {
    let p: Option<(i32, bool)> = sqlx::query_as("SELECT id, shuffle FROM moh_playlists WHERE owner_user_id = $1")
        .bind(uid)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?;
    let tracks = choices(db, p.map(|p| p.0), Some(uid)).await?;
    Ok((p.map(|p| p.1).unwrap_or(true), tracks))
}

#[derive(Deserialize)]
pub struct PersonalForm {
    #[serde(default)]
    shuffle: Option<String>,
    #[serde(default)]
    media: Vec<i32>,
}

pub async fn personal_save(State(app): State<AppState>, u: CurrentUser, MultiForm(f): MultiForm<PersonalForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !app.cfg.user_moh {
        return Err(AppError::Forbidden("Personal hold music is not enabled at this site.".into()));
    }
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO moh_playlists (owner_user_id, name, shuffle) VALUES ($1, 'personal', $2)
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL DO UPDATE SET shuffle = $2 RETURNING id",
)
    .bind(uid)
    .bind(f.shuffle.is_some())
    .fetch_one(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    set_tracks(&app.db, id, Some(uid), &f.media).await?;
    sync_playlist(&app.db, id).await.map_err(|e| AppError::BadRequest(format!("Hold music could not be written: {e}")))?;
    // devices pick up the new moh_suggest from the endpoint view
    let devices: Vec<String> = sqlx::query_scalar("SELECT sip_username FROM active_devices WHERE user_id = $1")
        .bind(uid)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    for d in &devices {
        app.asterisk_refresh(d).await;
    }
    db::audit(&app.db, &u.session.sub, "moh.personal", &format!("moh:{id}"), serde_json::json!({"tracks": f.media})).await?;
    Ok(Redirect::to("/media#hold-music").into_response())
}
