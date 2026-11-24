// SPDX-License-Identifier: GPL-3.0-or-later
//! Announcements and IVR (admin). Uploads are converted to Asterisk formats with ffmpeg.

use std::path::{Path as FsPath, PathBuf};

use askama::Template;
use axum::{
    Form,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use tokio::process::Command;

use crate::{
    AppError, AppState,
    admin::TargetInput,
    auth::Admin,
    db,
    ui::{Ctx, TARGET_DESC, TARGET_JOINS, TargetOptions, TargetSel, render, target_desc, target_joins, target_value},
};

pub const MEDIA_DIR: &str = "/media";
const MAX_UPLOAD: usize = 25 * 1024 * 1024;

pub fn upload_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(MAX_UPLOAD + 64 * 1024)
}


/// Input options for untrusted uploads: only real audio containers and local files, so a crafted
/// playlist (HLS/concat) cannot read other files or fetch URLs.
pub const SAFE_INPUT: [&str; 4] = [
    "-protocol_whitelist", "file",
    "-format_whitelist", "wav,mp3,ogg,flac,matroska,webm,mov,mp4,m4a,aac,wv,aiff",
];

/// Runs ffmpeg/ffprobe with a time limit (killed when it takes longer than 2 minutes).
pub async fn run(cmd: &mut Command) -> anyhow::Result<String> {
    cmd.kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("Conversion took too long"))??;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("Error"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Checks the file with ffprobe and creates sln16 (16 kHz), alaw (8 kHz) and an Ogg preview.
/// Returns the duration in seconds.
pub async fn convert(input: &FsPath, id: i32) -> anyhow::Result<f64> {
    let dur = run(Command::new("ffprobe").args(SAFE_INPUT).args(["-v", "error", "-select_streams", "a:0", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(input)).await?;
    let dur: f64 = dur.trim().parse().map_err(|_| anyhow::anyhow!("No audio track detected"))?;
    anyhow::ensure!(dur > 0.2 && dur <= 1800.0, "Duration must be between 0.2 s and 30 min");
    let base = PathBuf::from(MEDIA_DIR);
    let filter = "loudnorm=I=-18:TP=-2:LRA=11";
    for (rate, codec, path) in [
        ("16000", "pcm_s16le", base.join(format!("asterisk/{id}.sln16"))),
        ("8000", "pcm_alaw", base.join(format!("asterisk/{id}.alaw"))),
    ] {
        let fmt = if codec == "pcm_alaw" { "alaw" } else { "s16le" };
        run(Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(SAFE_INPUT)
            .arg("-i")
            .arg(input)
            .args(["-vn", "-af", filter, "-ac", "1", "-ar", rate, "-c:a", codec, "-f", fmt])
            .arg(&path))
        .await?;
    }
    run(Command::new("ffmpeg")
        .args(["-y", "-v", "error"])
        .args(SAFE_INPUT)
        .arg("-i")
        .arg(input)
        .args(["-vn", "-af", filter, "-ac", "1", "-c:a", "libvorbis", "-q:a", "3"])
        .arg(base.join(format!("preview/{id}.ogg"))))
    .await?;
    Ok(dur)
}

// --- Announcements --------------------------------------------------------------------------

#[derive(FromRow)]
pub struct SoundRow {
    id: i32,
    name: String,
    language: String,
    duration_s: Option<f64>,
    source_name: String,
    used: i64,
}

#[derive(Template)]
#[template(path = "admin_sounds.html")]
struct SoundsPage {
    c: Ctx,
    sounds: Vec<SoundRow>,
    playlists: Vec<crate::moh::PlaylistRow>,
}

pub async fn sounds(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let sounds = sqlx::query_as(
        "SELECT s.id, s.name, s.language, s.duration_s::float8 AS duration_s, s.source_name,
                (SELECT count(*) FROM targets t WHERE t.sound_id = s.id)
              + (SELECT count(*) FROM ivrs i WHERE i.greeting = s.id AND i.deleted_at IS NULL)
              + (SELECT count(*) FROM groups g WHERE g.announce_sound_id = s.id AND g.deleted_at IS NULL)
              + (SELECT count(*) FROM moh_media m WHERE m.sound_id = s.id)
              + (SELECT count(*) FROM app_media m JOIN apps a ON a.id = m.app_id AND a.deleted_at IS NULL WHERE m.sound_id = s.id) AS used
         FROM sounds s WHERE s.deleted_at IS NULL AND s.owner_user_id IS NULL ORDER BY s.name",
    )
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let playlists = crate::moh::company_playlists(&app.db).await?;
    render(SoundsPage { c: Ctx::new(&app, &a).section("sounds"), sounds, playlists })
}

pub async fn sound_upload(State(app): State<AppState>, Admin(a): Admin, mut mp: Multipart) -> Result<Response, AppError> {
    let mut name = String::new();
    let mut language = "de".to_string();
    let mut file: Option<(String, axum::body::Bytes)> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        match field.name() {
            Some("name") => name = field.text().await.unwrap_or_default().trim().to_string(),
            Some("language") => language = field.text().await.unwrap_or_default(),
            Some("file") => {
                let fname = field.file_name().unwrap_or("upload").to_string();
                let data = field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
                file = Some((fname, data));
            }
            _ => {}
        }
    }
    let (fname, data) = file.filter(|(_, d)| !d.is_empty()).ok_or_else(|| AppError::BadRequest("No file.".into()))?;
    if name.is_empty() {
        name = fname.rsplit_once('.').map(|(n, _)| n.to_string()).unwrap_or(fname.clone());
    }
    let name: String = name.chars().take(60).collect();
    let language = if language == "en" { "en" } else { "de" };

    let id: i32 = sqlx::query_scalar(
        "INSERT INTO sounds (name, language, file_path, source_name) VALUES ($1, $2, '', $3) RETURNING id",
    )
    .bind(&name)
    .bind(language)
    .bind(&fname)
    .fetch_one(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let tmp = std::env::temp_dir().join(format!("upload-{id}"));
    tokio::fs::write(&tmp, &data).await.map_err(anyhow::Error::from)?;
    let res = convert(&tmp, id).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    match res {
        Ok(dur) => {
            sqlx::query("UPDATE sounds SET file_path = id::text, duration_s = $2 WHERE id = $1")
                .bind(id)
                .bind(dur)
                .execute(&app.db)
                .await
                .map_err(anyhow::Error::from)?;
            db::audit(&app.db, &a.session.sub, "sound.add", &format!("sound:{id}"), serde_json::json!({"name": name, "file": fname})).await?;
            Ok(Redirect::to("/admin/sounds").into_response())
        }
        Err(e) => {
            sqlx::query("DELETE FROM sounds WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
            Err(AppError::BadRequest(format!("File could not be converted: {e}")))
        }
    }
}

pub async fn sound_preview(State(app): State<AppState>, Admin(_): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT id FROM sounds WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    exists.ok_or(AppError::NotFound)?;
    let data = tokio::fs::read(format!("{MEDIA_DIR}/preview/{id}.ogg")).await.map_err(|_| AppError::NotFound)?;
    Ok(([(header::CONTENT_TYPE, "audio/ogg"), (header::CACHE_CONTROL, "private, max-age=3600")], Body::from(data)).into_response())
}

#[derive(Deserialize)]
pub struct SoundEditById {
    id: i32,
    name: String,
}

/// POST /admin/sounds/edit: the edit form of the audio library (Edit button fills the id)
pub async fn sound_edit(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<SoundEditById>) -> Result<Response, AppError> {
    rename(&app, &a.session.sub, f.id, &f.name).await
}

async fn rename(app: &AppState, actor: &str, id: i32, name: &str) -> Result<Response, AppError> {
    let name: String = name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let n = sqlx::query("UPDATE sounds SET name = $2 WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .bind(&name)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("The audio file no longer exists. Please reload the page.".into()));
    }
    db::audit(&app.db, actor, "sound.rename", &format!("sound:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to("/admin/sounds").into_response())
}

pub async fn sound_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let n = sqlx::query(
        "UPDATE sounds s SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM targets t WHERE t.sound_id = s.id)
           AND NOT EXISTS (SELECT 1 FROM ivrs i WHERE i.greeting = s.id AND i.deleted_at IS NULL)
           AND NOT EXISTS (SELECT 1 FROM groups g WHERE g.announce_sound_id = s.id AND g.deleted_at IS NULL)
           AND NOT EXISTS (SELECT 1 FROM moh_media m WHERE m.sound_id = s.id)
           AND NOT EXISTS (SELECT 1 FROM app_media m JOIN apps a ON a.id = m.app_id AND a.deleted_at IS NULL WHERE m.sound_id = s.id)",
    )
    .bind(id)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("The audio file is still in use.".into()));
    }
    for f in [format!("asterisk/{id}.sln16"), format!("asterisk/{id}.alaw"), format!("preview/{id}.ogg")] {
        let _ = tokio::fs::remove_file(format!("{MEDIA_DIR}/{f}")).await;
    }
    db::audit(&app.db, &a.session.sub, "sound.delete", &format!("sound:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/sounds").into_response())
}

// --- IVR ----------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct IvrRow {
    id: i32,
    number: Option<i16>,
    invalid_mode: String,
    name: String,
    greeting: Option<i32>,
    greeting_name: Option<String>,
    timeout_s: i16,
    repeat: i16,
    timeout_kind: Option<String>,
    timeout_value: Option<String>,
    timeout_desc: Option<String>,
    timeout_id: Option<i32>,
    options: i64,
}

#[derive(FromRow)]
pub struct OptionRow {
    digit: String,
    kind: String,
    value: Option<String>,
    target_desc: String,
}

impl OptionRow {
    fn value_str(&self) -> &str {
        self.value.as_deref().unwrap_or_default()
    }
}

/// a key in the digit select of the add/edit form
pub struct DigitOpt {
    digit: &'static str,
    /// assigned to a target already (shown as such; the server refuses it unless it is the edited key)
    taken: bool,
    /// preselected when adding: the first free key
    first_free: bool,
}

/// all IVR keys in keypad order
const DIGITS: [&str; 12] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "*", "#"];

const IVR_SELECT: &str = concat!(
    "SELECT i.id, i.number, i.invalid_mode, i.name, i.greeting, s.name AS greeting_name, i.timeout_s, i.repeat,
        t.kind::text AS timeout_kind, ",
    target_value!(),
    " AS timeout_value, ",
    target_desc!(),
    " AS timeout_desc,
        t.id AS timeout_id,
        (SELECT count(*) FROM ivr_options o WHERE o.ivr_id = i.id) AS options
    FROM ivrs i LEFT JOIN sounds s ON s.id = i.greeting
    LEFT JOIN targets t ON t.id = i.timeout_target ",
    target_joins!(),
    " WHERE i.deleted_at IS NULL"
);

#[derive(Template)]
#[template(path = "admin_ivrs.html")]
struct IvrsPage {
    c: Ctx,
    ivrs: Vec<IvrRow>,
}

#[derive(Template)]
#[template(path = "admin_ivr.html")]
struct IvrPage {
    c: Ctx,
    ivr: IvrRow,
    options: Vec<OptionRow>,
    opts: TargetOptions,
    timeout: TargetSel,
    newopt: TargetSel,
    digits: Vec<DigitOpt>,
    all_taken: bool,
}

impl IvrRow {
    fn number_str(&self) -> String {
        self.number.map(|n| n.to_string()).unwrap_or_default()
    }

    fn greeting_is(&self, id: &str) -> bool {
        self.greeting.map(|g| g.to_string()) == Some(id.to_string())
    }
}

pub async fn ivrs(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let ivrs = sqlx::query_as(&format!("{IVR_SELECT} ORDER BY i.name"))
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    render(IvrsPage { c: Ctx::new(&app, &a).section("ivrs"), ivrs })
}

#[derive(Deserialize)]
pub struct NewIvr {
    name: String,
}

pub async fn ivr_create(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<NewIvr>) -> Result<Response, AppError> {
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let id: i32 = sqlx::query_scalar("INSERT INTO ivrs (name) VALUES ($1) RETURNING id")
        .bind(&name)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "ivr.add", &format!("ivr:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to(&format!("/admin/ivrs/{id}")).into_response())
}

async fn load_ivr(db: &PgPool, id: i32) -> Result<IvrRow, AppError> {
    sqlx::query_as(&format!("{IVR_SELECT} AND i.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)
}

pub async fn ivr(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let ivr = load_ivr(&app.db, id).await?;
    let mut options: Vec<OptionRow> = sqlx::query_as(&format!(
        "SELECT o.digit, t.kind::text AS kind, {} AS value, {TARGET_DESC} AS target_desc
         FROM ivr_options o JOIN targets t ON t.id = o.target_id {TARGET_JOINS}
         WHERE o.ivr_id = $1",
        target_value!()
    ))
    .bind(id)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    options.sort_by_key(|o| DIGITS.iter().position(|d| *d == o.digit));
    let first = DIGITS.iter().copied().find(|d| !options.iter().any(|o| o.digit == *d));
    let digits: Vec<DigitOpt> = DIGITS
        .iter()
        .map(|d| DigitOpt { digit: d, taken: options.iter().any(|o| o.digit == *d), first_free: first == Some(*d) })
        .collect();
    let all_taken = first.is_none();
    let timeout = TargetSel::new("timeout_", ivr.timeout_kind.clone(), ivr.timeout_value.clone(), "hang up");
    let newopt = TargetSel::new("", None, None, "no target");
    render(IvrPage {
        c: Ctx::new(&app, &a).section("ivrs"),
        ivr,
        options,
        opts: TargetOptions::load(&app.db).await?,
        timeout,
        newopt,
        digits,
        all_taken,
    })
}

#[derive(Deserialize)]
pub struct IvrForm {
    name: String,
    #[serde(default)]
    number: String,
    #[serde(default)]
    invalid_mode: String,
    greeting: String,
    timeout_s: i16,
    repeat: i16,
    timeout_kind: String,
    #[serde(default)]
    timeout_value: String,
}

pub async fn ivr_save(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<IvrForm>) -> Result<Response, AppError> {
    let old = load_ivr(&app.db, id).await?;
    if !(1..=30).contains(&f.timeout_s) || !(0..=10).contains(&f.repeat) {
        return Err(AppError::BadRequest("Timeout 1–30 s, Wiederholungen 0–10.".into()));
    }
    if f.timeout_kind == "ivr" && f.timeout_value.trim() == id.to_string() {
        return Err(AppError::BadRequest("The IVR cannot point to itself; use repetitions for that.".into()));
    }
    let t = crate::admin::save_target(&app.db, old.timeout_id, &TargetInput { kind: f.timeout_kind, value: f.timeout_value }).await?;
    let number: Option<i16> = match f.number.trim() {
        "" => None,
        n => Some(crate::numbers::check(&app.db, &app.cfg.numbers, "ivr", n, Some(id)).await?),
    };
    sqlx::query("UPDATE ivrs SET name = $2, greeting = $3, timeout_s = $4, repeat = $5, timeout_target = $6, number = $7,
                invalid_mode = $8 WHERE id = $1")
        .bind(id)
        .bind(f.name.trim())
        .bind(f.greeting.parse::<i32>().ok())
        .bind(f.timeout_s)
        .bind(f.repeat)
        .bind(t)
        .bind(number)
        .bind(if f.invalid_mode == "ignore" { "ignore" } else { "announce" })
        .execute(&app.db)
        .await
        .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &a.session.sub, "ivr.edit", &format!("ivr:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/admin/ivrs/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct OptionForm {
    digit: String,
    /// the key's digit before the change (editing a row); empty when adding a key
    #[serde(default)]
    old_digit: String,
    kind: String,
    #[serde(default)]
    value: String,
}

fn check_digit(d: &str) -> Result<String, AppError> {
    let d = d.trim();
    if DIGITS.contains(&d) {
        Ok(d.to_string())
    } else {
        Err(AppError::BadRequest("Key: 0–9, * or #.".into()))
    }
}

/// Adds a key (no `old_digit`; refused if the key is taken) or changes one: its target and,
/// with a different `digit`, the key itself (refused if the new key is taken).
pub async fn ivr_option_set(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<OptionForm>) -> Result<Response, AppError> {
    load_ivr(&app.db, id).await?;
    let digit = check_digit(&f.digit)?;
    let old = if f.old_digit.trim().is_empty() { None } else { Some(check_digit(&f.old_digit)?) };
    if f.kind == "none" || f.kind.is_empty() {
        return Err(AppError::BadRequest("Please select a target (Remove deletes a key).".into()));
    }
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    // serialize key changes of this IVR (two admins assigning the same key); NO KEY UPDATE, as the
    // targets row written by save_target (another connection) may reference this IVR (key → own
    // menu), and its foreign key check takes KEY SHARE, which FOR UPDATE would block forever
    sqlx::query("SELECT id FROM ivrs WHERE id = $1 FOR NO KEY UPDATE").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    let taken: Option<i32> = sqlx::query_scalar("SELECT target_id FROM ivr_options WHERE ivr_id = $1 AND digit = $2")
        .bind(id)
        .bind(&digit)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let existing = match &old {
        None if taken.is_some() => {
            return Err(AppError::BadRequest(format!("Key {digit} is already assigned; change it in its row.")));
        }
        None => None,
        Some(o) if *o != digit && taken.is_some() => {
            return Err(AppError::BadRequest(format!("Key {digit} is already assigned.")));
        }
        Some(o) => Some(
            sqlx::query_scalar::<_, i32>("SELECT target_id FROM ivr_options WHERE ivr_id = $1 AND digit = $2")
                .bind(id)
                .bind(o)
                .fetch_optional(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?
                .ok_or_else(|| AppError::BadRequest(format!("Key {o} no longer exists. Please reload the page.")))?,
        ),
    };
    let t = crate::admin::save_target(&app.db, existing, &TargetInput { kind: f.kind.clone(), value: f.value.clone() })
        .await?
        .ok_or_else(|| AppError::BadRequest("Please select a target.".into()))?;
    // $1 IVR, $2 key before the change (= new key when adding), $3 new key, $4 target
    let sql = if old.is_some() {
        "UPDATE ivr_options SET digit = $3, target_id = $4 WHERE ivr_id = $1 AND digit = $2"
    } else {
        "INSERT INTO ivr_options (ivr_id, digit, target_id) SELECT $1, $3, $4 WHERE $2 = $3"
    };
    sqlx::query(sql)
    .bind(id)
    .bind(old.as_deref().unwrap_or(&digit))
    .bind(&digit)
    .bind(t)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(
        &app.db,
        &a.session.sub,
        "ivr.option",
        &format!("ivr:{id}"),
        serde_json::json!({"digit": digit, "old_digit": old, "kind": f.kind, "value": f.value}),
    )
    .await?;
    Ok(Redirect::to(&format!("/admin/ivrs/{id}")).into_response())
}

/// Removes a key (button in its row; the row's `old_digit` names it).
pub async fn ivr_option_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<OptionForm>) -> Result<Response, AppError> {
    load_ivr(&app.db, id).await?;
    let digit = check_digit(if f.old_digit.trim().is_empty() { &f.digit } else { &f.old_digit })?;
    remove_option(&app, &a.session.sub, id, &digit).await
}

async fn remove_option(app: &AppState, actor: &str, id: i32, digit: &str) -> Result<Response, AppError> {
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    // same lock as ivr_option_set, so a concurrent edit does not lose its target row
    sqlx::query("SELECT id FROM ivrs WHERE id = $1 FOR NO KEY UPDATE").bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    let t: Option<i32> = sqlx::query_scalar("DELETE FROM ivr_options WHERE ivr_id = $1 AND digit = $2 RETURNING target_id")
        .bind(id)
        .bind(digit)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(t) = t {
        // the key's own target row; nothing else points to it
        sqlx::query("DELETE FROM targets WHERE id = $1").bind(t).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, actor, "ivr.option", &format!("ivr:{id}"), serde_json::json!({"digit": digit, "kind": "none"})).await?;
    Ok(Redirect::to(&format!("/admin/ivrs/{id}")).into_response())
}

pub async fn ivr_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM targets WHERE ivr_id = $1")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if used > 0 {
        return Err(AppError::BadRequest("The IVR is still used as a target.".into()));
    }
    sqlx::query("UPDATE ivrs SET deleted_at = now() WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "ivr.delete", &format!("ivr:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/ivrs").into_response())
}
