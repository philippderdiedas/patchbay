// SPDX-License-Identifier: GPL-3.0-or-later
//! Voicemail: internal webhook from Asterisk, conversion, notification mail, MWI and the web UI.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Form, Router,
    body::Body,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
    routing::post,
};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Attachment, MultiPart, SinglePart, header::ContentType},
};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use tokio::process::Command;

use crate::{AppError, AppState, auth::CurrentUser, db, ui::{Ctx, render}};

const SPOOL: &str = "/voicemail";
const STORE: &str = "/media/voicemail";
const RETENTION_DAYS: i32 = 90;

// --- Internal webhook (127.0.0.1:8081, called by the Asterisk hangup handler) -----------

pub fn internal_router(app: AppState) -> Router {
    Router::new()
        .route("/internal/voicemail", post(new_message))
        .route("/internal/voicemail/list", post(phone_list))
        .route("/internal/voicemail/action", post(phone_action))
        .with_state(app)
}

#[derive(Deserialize, serde::Serialize, Debug)]
struct NewMessage {
    id: String,
    owner_type: String,
    owner: String,
    #[serde(default)]
    caller: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    did: String,
    /// Linkedid of the call (call tracking)
    #[serde(default)]
    call: String,
}

async fn new_message(State(app): State<AppState>, Form(m): Form<NewMessage>) -> &'static str {
    let id = m.id.clone();
    let form = serde_urlencoded::to_string(&m).unwrap_or_default();
    if let Err(e) = register(&app, m).await {
        tracing::warn!("voicemail {id}: {e:#} (retried by the sweep)");
        // keep the mailbox information next to the recording, the sweep retries it
        if valid_id(&id) && !form.is_empty() {
            let _ = tokio::fs::write(format!("{SPOOL}/{id}.meta"), form).await;
        }
    }
    "ok"
}

async fn duration(path: &std::path::Path) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().parse()?)
}

fn valid_id(id: &str) -> bool {
    id.len() > 2 && id.starts_with("vm") && id[2..].chars().all(|c| c.is_ascii_digit())
}

/// Removes the spool files of a message (recording and the sidecar the dialplan writes when the
/// webhook could not be delivered).
async fn remove_spool(id: &str) {
    let _ = tokio::fs::remove_file(format!("{SPOOL}/{id}.wav")).await;
    let _ = tokio::fs::remove_file(format!("{SPOOL}/{id}.meta")).await;
}

async fn register(app: &AppState, m: NewMessage) -> Result<()> {
    anyhow::ensure!(valid_id(&m.id), "invalid id");
    let wav = PathBuf::from(format!("{SPOOL}/{}.wav", m.id));
    // already stored (webhook and sweep, or a repeated webhook): only the spool is left over
    let known: Option<i32> = sqlx::query_scalar("SELECT id FROM voicemails WHERE uuid = $1").bind(&m.id).fetch_optional(&app.db).await?;
    if known.is_some() {
        remove_spool(&m.id).await;
        return Ok(());
    }
    let dur = duration(&wav).await.unwrap_or(0.0);
    if dur < 1.5 {
        // Caller hung up during the greeting or right after the beep
        remove_spool(&m.id).await;
        return Ok(());
    }
    let (owner_type, owner_id): (&str, i32) = match m.owner_type.as_str() {
        "user" => ("user", sqlx::query_scalar("SELECT id FROM users WHERE extension::text = $1 AND deleted_at IS NULL")
            .bind(&m.owner).fetch_one(&app.db).await.context("unknown user")?),
        "group" => ("group", sqlx::query_scalar("SELECT id FROM groups WHERE number::text = $1 AND deleted_at IS NULL")
            .bind(&m.owner).fetch_one(&app.db).await.context("unknown call group")?),
        _ => anyhow::bail!("invalid owner type"),
    };
    let mp3 = PathBuf::from(format!("{STORE}/{}.mp3", m.id));
    let out = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(&wav)
        .args(["-ac", "1", "-c:a", "libmp3lame", "-q:a", "6"])
        .arg(&mp3)
        .output()
        .await?;
    anyhow::ensure!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    // only patchbay-web and Asterisk (group 999, setgid directory) read messages
    tokio::fs::set_permissions(&mp3, { use std::os::unix::fs::PermissionsExt; std::fs::Permissions::from_mode(0o640) }).await?;
    // copy for listening by phone (*97); a failure here is retried by the sweep's backfill
    if let Err(e) = write_phone_copy(&wav, &mp3).await {
        tracing::warn!("voicemail {}: phone copy: {e:#}", m.id);
    }

    let caller = Some(m.caller.trim()).filter(|c| !c.is_empty() && *c != "anonymous");
    let name = Some(m.name.trim()).filter(|n| !n.is_empty() && Some(*n) != caller);
    let did_id: Option<i32> = sqlx::query_scalar("SELECT id FROM dids WHERE e164 = $1").bind(&m.did).fetch_optional(&app.db).await?;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO voicemails (uuid, owner_type, owner_id, caller_e164, caller_name, did_id, duration_s, file_path)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&m.id)
    .bind(owner_type)
    .bind(owner_id)
    .bind(caller)
    .bind(name)
    .bind(did_id)
    .bind(dur.round() as i32)
    .bind(mp3.to_string_lossy().as_ref())
    .fetch_one(&app.db)
    .await?;
    // only now: a failure above leaves the recording for the sweep
    remove_spool(&m.id).await;
    tracing::info!("voicemail {id} for {owner_type} {} ({dur:.0} s)", m.owner);
    {
        use patchbay_notify::event::{Party, Target, TargetKind};
        app.events.publish(patchbay_notify::PbxEvent::VoicemailNew {
            id,
            call_id: Some(m.call.clone()).filter(|c| !c.is_empty()),
            owner: Target::new(if owner_type == "user" { TargetKind::User } else { TargetKind::Group }, m.owner.clone()),
            caller: Party { number: caller.unwrap_or_default().to_string(), name: name.map(String::from) },
            duration_s: dur.round() as i32,
        });
    }

    update_mwi_for(app, id).await;
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = notify(&app, id).await {
            tracing::warn!("voicemail mail {id}: {e:#}");
        }
    });
    Ok(())
}

// --- Copy for Asterisk (listening by phone, *97) ---------------------------------------------

/// Asterisk cannot play MP3: every message also has an 8 kHz mono WAV next to the MP3
/// (`/media/voicemail/<uuid>.wav`, the same path in the Asterisk container, read-only there).
fn phone_copy(mp3: &str) -> PathBuf {
    std::path::Path::new(mp3).with_extension("wav")
}

/// Converts `src` (spool recording or the MP3) into the phone copy of `mp3`. Written under a
/// temporary name and renamed, so Asterisk never plays a half-written file; mode 0644 because
/// Asterisk (UID 999) reads it as "other".
async fn write_phone_copy(src: &std::path::Path, mp3: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dst = phone_copy(&mp3.to_string_lossy());
    let tmp = dst.with_extension("tmp.wav");
    let out = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(src)
        .args(["-vn", "-ac", "1", "-ar", "8000", "-c:a", "pcm_s16le"])
        .arg(&tmp)
        .output()
        .await?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    }
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640)).await?;
    tokio::fs::rename(&tmp, &dst).await?;
    Ok(())
}

/// Removes the stored audio of a message (MP3 and phone copy).
pub(crate) async fn remove_audio(mp3: &str) {
    let _ = tokio::fs::remove_file(mp3).await;
    let _ = tokio::fs::remove_file(phone_copy(mp3)).await;
}

/// Creates missing phone copies from the MP3s (messages from before *97, or a failed
/// conversion). Idempotent; runs with the sweep (startup and every 10 minutes).
async fn backfill_phone_copies(app: &AppState) -> Result<()> {
    let files: Vec<String> = sqlx::query_scalar("SELECT file_path FROM voicemails WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    let mut made = 0;
    for f in files {
        let mp3 = PathBuf::from(&f);
        if tokio::fs::metadata(phone_copy(&f)).await.is_ok() || tokio::fs::metadata(&mp3).await.is_err() {
            continue;
        }
        match write_phone_copy(&mp3, &mp3).await {
            Ok(()) => made += 1,
            Err(e) => tracing::warn!("voicemail phone copy {f}: {e:#}"),
        }
    }
    if made > 0 {
        tracing::info!("voicemail: {made} phone copies created");
    }
    Ok(())
}

// --- Listening by phone (*97): internal endpoints for the dialplan ----------------------------

/// At most this many messages are offered by phone (the dialplan keeps the list in one variable).
const PHONE_MAX: i64 = 40;

#[derive(Deserialize)]
struct PhoneForm {
    ext: String,
    #[serde(default)]
    id: i32,
    #[serde(default)]
    action: String,
}

#[derive(FromRow)]
struct PhoneMsg {
    id: i32,
    new: bool,
    epoch: i64,
    caller: Option<String>,
    file_path: String,
}

/// `ok,<new>,<old>,<time zone>,<entry>,…` with entries `<id>:<1 new|0 heard>:<unix time>:<caller
/// digits or empty>:<file>` (file = name of the phone copy without extension in /media/voicemail).
/// Same messages as the web UI (own and call group mailboxes): new ones oldest first, then heard
/// ones newest first. Counts cover all messages, the list at most `PHONE_MAX`. "error" for an
/// unknown extension.
async fn phone_list(State(app): State<AppState>, Form(f): Form<PhoneForm>) -> String {
    let Some(uid) = crate::selfservice::uid_by_ext(&app, &f.ext).await else { return "error".into() };
    let res: Result<String> = async {
        let (new, old): (i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE v.heard_at IS NULL), count(*) FILTER (WHERE v.heard_at IS NOT NULL) FROM voicemails v WHERE {ACCESS}"
        ))
        .bind(uid)
        .fetch_one(&app.db)
        .await?;
        let msgs: Vec<PhoneMsg> = sqlx::query_as(&format!(
            "SELECT v.id, v.heard_at IS NULL AS new, extract(epoch FROM v.received_at)::int8 AS epoch,
                    v.caller_e164 AS caller, v.file_path
             FROM voicemails v WHERE {ACCESS}
             ORDER BY v.heard_at IS NULL DESC,
                      CASE WHEN v.heard_at IS NULL THEN v.received_at END ASC,
                      v.received_at DESC
             LIMIT $2"
        ))
        .bind(uid)
        .bind(PHONE_MAX)
        .fetch_all(&app.db)
        .await?;
        Ok(phone_list_text(new, old, &app.cfg.timezone, &app.cfg.country_code, &msgs))
    }
    .await;
    res.unwrap_or_else(|e| {
        tracing::warn!("voicemail list for {}: {e:#}", f.ext);
        "error".into()
    })
}

fn phone_list_text(new: i64, old: i64, tz: &str, cc: &str, msgs: &[PhoneMsg]) -> String {
    let mut out = format!("ok,{new},{old},{}", tz.replace([',', ':'], ""));
    for m in msgs {
        // only digits for SayDigits: +49301234 -> 0301234, other countries 00…
        let caller = m.caller.as_deref().map(|c| crate::admin::normalise_with(c, cc, &[])).unwrap_or_default();
        let caller: String = caller.chars().filter(char::is_ascii_digit).collect();
        let file = std::path::Path::new(&m.file_path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let file: String = file.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        out.push_str(&format!(",{}:{}:{}:{caller}:{file}", m.id, u8::from(m.new), m.epoch));
    }
    out
}

/// heard / delete from the phone menu, through `apply` (access check, audit, MWI). A message that
/// is already heard stays as it is (playing it again does not change who heard it when).
/// "ok" / "denied" (no access, unknown extension) / "error".
async fn phone_action(State(app): State<AppState>, Form(f): Form<PhoneForm>) -> &'static str {
    if !matches!(f.action.as_str(), "heard" | "delete") {
        return "error";
    }
    let Some(uid) = crate::selfservice::uid_by_ext(&app, &f.ext).await else { return "denied" };
    if f.action == "heard" {
        let heard: Option<bool> = sqlx::query_scalar("SELECT heard_at IS NOT NULL FROM voicemails WHERE id = $1 AND deleted_at IS NULL")
            .bind(f.id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        if heard == Some(true) && accessible(&app.db, uid, f.id).await.is_ok() {
            return "ok";
        }
    }
    match apply(&app, uid, f.id, &f.action, &format!("phone:user-id:{uid}")).await {
        Ok(true) => "ok",
        Ok(false) => "denied",
        Err(e) => {
            tracing::warn!("voicemail {} by phone ({}): {e:#}", f.action, f.ext);
            "error"
        }
    }
}

// --- Notification mail ---------------------------------------------------------------------

#[derive(FromRow)]
struct MailInfo {
    caller_e164: Option<String>,
    caller_name: Option<String>,
    duration_s: Option<i32>,
    file_path: String,
    owner_label: String,
    received: String,
}

async fn notify(app: &AppState, id: i32) -> Result<()> {
    let (Some(smtp), Some(from)) = (&app.cfg.smtp, &app.cfg.mail_from) else {
        return Ok(());
    };
    let info: MailInfo = sqlx::query_as(
        "SELECT v.caller_e164, v.caller_name, v.duration_s, v.file_path,
                CASE v.owner_type WHEN 'user' THEN (SELECT extension || ' ' || display_name FROM users WHERE id = v.owner_id)
                     ELSE (SELECT number || ' ' || name FROM groups WHERE id = v.owner_id) END AS owner_label,
                to_char(v.received_at, 'DD.MM.YYYY HH24:MI') AS received
         FROM voicemails v WHERE v.id = $1",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await?;
    // Personal delivery setting (web UI, with the chat bot): mail, chat, both or none. "chat"
    // falls back to mail while the bot is off or the user has not linked a chat account.
    let recipients: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT u.email, u.ui_language FROM voicemails v JOIN users u ON
             (v.owner_type = 'user' AND u.id = v.owner_id)
          OR (v.owner_type = 'group' AND u.id IN (SELECT user_id FROM group_members WHERE group_id = v.owner_id AND enabled))
         LEFT JOIN bot_user_prefs p ON p.user_id = u.id
         WHERE v.id = $1 AND u.email IS NOT NULL AND u.deleted_at IS NULL
           AND (COALESCE(p.vm_delivery, 'mail') IN ('mail', 'both')
                OR (p.vm_delivery = 'chat' AND (NOT $2 OR NOT EXISTS (SELECT 1 FROM bot_links l WHERE l.user_id = u.id))))",
    )
    .bind(id)
    .bind(app.bot.enabled())
    .fetch_all(&app.db)
    .await?;
    if recipients.is_empty() {
        return Ok(());
    }
    let dur = info.duration_s.unwrap_or(0);
    let audio = tokio::fs::read(&info.file_path).await?;
    let (host, port) = smtp.split_once(':').unwrap_or((smtp.as_str(), "25"));
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host).port(port.parse()?).build();
    // One mail per language, in the recipients' UI language (default English)
    for german in [false, true] {
        let to: Vec<&String> = recipients
            .iter()
            .filter(|(_, l)| (l.as_deref() == Some("de")) == german)
            .map(|(e, _)| e)
            .collect();
        if to.is_empty() {
            continue;
        }
        let unknown = if german { "unbekannt" } else { "unknown" };
        let caller = match (&info.caller_name, &info.caller_e164) {
            (Some(n), Some(c)) => format!("{n} ({c})"),
            (None, Some(c)) => c.clone(),
            (Some(n), None) => n.clone(),
            (None, None) => unknown.into(),
        };
        let (subject, body) = if german {
            (
                format!("Sprachnachricht: {caller} ({dur} s)"),
                format!(
                    "Neue Sprachnachricht für {owner}\nVon: {caller}\nEingang: {at}\nDauer: {dur} s\n\n{url}/voicemail\n",
                    owner = info.owner_label, at = info.received, url = app.cfg.public_url
                ),
            )
        } else {
            (
                format!("Voicemail: {caller} ({dur} s)"),
                format!(
                    "New voicemail for {owner}\nFrom: {caller}\nReceived: {at}\nDuration: {dur} s\n\n{url}/voicemail\n",
                    owner = info.owner_label, at = info.received, url = app.cfg.public_url
                ),
            )
        };
        let mut builder = Message::builder().from(from.parse()?).subject(subject);
        for r in &to {
            builder = builder.to(r.parse()?);
        }
        let msg = builder.multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body))
                .singlepart(Attachment::new(format!("voicemail-{id}.mp3")).body(audio.clone(), ContentType::parse("audio/mpeg")?)),
        )?;
        mailer.send(msg).await?;
    }
    Ok(())
}

// --- MWI (message waiting indicator) via ARI / res_mwi_external ---------------------------

/// Sets new/old message counts for every user mailbox ("<ext>@pbx"); group messages count for
/// all members of the call group.
pub async fn update_mwi(app: &AppState) {
    if let Err(e) = update_mwi_inner(app, None).await {
        tracing::warn!("MWI update failed: {e:#}");
    }
}

/// A voicemail was marked heard/unheard or deleted: the chat bot brings its posts in line.
pub fn changed(app: &AppState, voicemail_id: i32) {
    app.events.publish(patchbay_notify::PbxEvent::VoicemailChanged { id: voicemail_id });
}

/// Like `update_mwi`, but only the mailboxes a message counts for (its owner or the members of
/// its call group).
pub async fn update_mwi_for(app: &AppState, voicemail_id: i32) {
    if let Err(e) = update_mwi_inner(app, Some(voicemail_id)).await {
        tracing::warn!("MWI update failed: {e:#}");
    }
}

/// After a number change: clear the lamp state of the old mailbox, set all current ones.
pub async fn mailbox_moved(app: &AppState, old_ext: i16) {
    if let Ok(secret) = std::env::var("PBX_ARI_SECRET_FILE").map_err(anyhow::Error::from).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
        let http = reqwest::Client::new();
        let _ = http
            .delete(format!("http://127.0.0.1:8088/ari/mailboxes/{old_ext}@pbx"))
            .basic_auth("pbxweb", Some(secret.trim()))
            .timeout(Duration::from_secs(3))
            .send()
            .await;
    }
    update_mwi(app).await;
}

/// `only_vm`: limit the update to the mailboxes of this message. One failing mailbox does not stop
/// the others; the error reports how many failed.
async fn update_mwi_inner(app: &AppState, only_vm: Option<i32>) -> Result<()> {
    let secret = std::env::var("PBX_ARI_SECRET_FILE").ok().and_then(|p| std::fs::read_to_string(p).ok()).context("PBX_ARI_SECRET_FILE")?;
    let counts: Vec<(i16, i64, i64)> = sqlx::query_as(
        "SELECT u.extension,
                count(v.id) FILTER (WHERE v.heard_at IS NULL) AS new,
                count(v.id) FILTER (WHERE v.heard_at IS NOT NULL) AS old
         FROM users u LEFT JOIN voicemails v ON v.deleted_at IS NULL AND (
                 (v.owner_type = 'user' AND v.owner_id = u.id)
              OR (v.owner_type = 'group' AND v.owner_id IN (SELECT group_id FROM group_members WHERE user_id = u.id)))
         WHERE u.deleted_at IS NULL
           AND ($1::int IS NULL OR u.id IN (
                 SELECT CASE WHEN m.owner_type = 'user' THEN m.owner_id END FROM voicemails m WHERE m.id = $1
                 UNION SELECT gm.user_id FROM voicemails m JOIN group_members gm ON gm.group_id = m.owner_id
                       WHERE m.id = $1 AND m.owner_type = 'group'))
         GROUP BY u.extension",
    )
    .bind(only_vm)
    .fetch_all(&app.db)
    .await?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
    let (total, mut failed) = (counts.len(), 0);
    for (ext, new, old) in counts {
        let res = http
            .put(format!("http://127.0.0.1:8088/ari/mailboxes/{ext}@pbx?oldMessages={old}&newMessages={new}"))
            .basic_auth("pbxweb", Some(secret.trim()))
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Err(e) = res {
            failed += 1;
            tracing::debug!("MWI {ext}@pbx: {e}");
        }
    }
    anyhow::ensure!(failed == 0, "{failed} of {total} mailboxes not updated");
    Ok(())
}

// --- Retention -------------------------------------------------------------------------------

pub fn spawn_cleanup(app: AppState) {
    tokio::spawn(async move {
        loop {
            let res: Result<()> = async {
                let old: Vec<(i32, String)> = sqlx::query_as(
                    "UPDATE voicemails SET deleted_at = now()
                     WHERE deleted_at IS NULL AND received_at < now() - make_interval(days => $1) RETURNING id, file_path",
                )
                .bind(RETENTION_DAYS)
                .fetch_all(&app.db)
                .await?;
                for (id, f) in &old {
                    remove_audio(f).await;
                    changed(&app, *id);
                }
                if !old.is_empty() {
                    update_mwi(&app).await;
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("voicemail cleanup: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

// --- Sweep: messages the webhook did not deliver ---------------------------------------------

/// A recording is complete when it has not been written to for this long (Record() writes
/// continuously and stops after 180 s).
const SWEEP_MIN_AGE: Duration = Duration::from_secs(300);
const SWEEP_GIVE_UP: Duration = Duration::from_secs(7 * 24 * 3600);

/// At startup and every 10 minutes: registers recordings left in the spool. When the webhook
/// fails (patchbay-web down), the dialplan writes the form data next to the recording (`<id>.meta`);
/// patchbay-web does the same when registering fails. Recordings without it, or still failing after
/// `SWEEP_GIVE_UP`, are renamed to `<id>.orphan.wav` (logged once) and kept for manual recovery.
pub fn spawn_sweep(app: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Err(e) = sweep(&app).await {
                tracing::warn!("voicemail sweep: {e:#}");
            }
            if let Err(e) = backfill_phone_copies(&app).await {
                tracing::warn!("voicemail phone copies: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

/// File in the spool: (message id, extension) for `vm<digits>.wav|.meta`
fn spool_entry(name: &str) -> Option<(&str, &str)> {
    let (id, ext) = name.split_once('.')?;
    (valid_id(id) && matches!(ext, "wav" | "meta")).then_some((id, ext))
}

async fn sweep(app: &AppState) -> Result<()> {
    let mut dir = tokio::fs::read_dir(SPOOL).await.context(SPOOL)?;
    while let Some(e) = dir.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        let Some((id, ext)) = spool_entry(&name) else { continue };
        let age = e.metadata().await?.modified()?.elapsed().unwrap_or_default();
        if age < SWEEP_MIN_AGE {
            continue;
        }
        let wav = format!("{SPOOL}/{id}.wav");
        let meta = format!("{SPOOL}/{id}.meta");
        if ext == "meta" {
            // the webhook succeeded after all (sidecar written after the recording was taken)
            if tokio::fs::metadata(&wav).await.is_err() {
                let _ = tokio::fs::remove_file(&meta).await;
            }
            continue;
        }
        let orphan = |why: &str| {
            tracing::warn!("voicemail sweep: {wav} {why}, kept as {id}.orphan.wav");
            format!("{SPOOL}/{id}.orphan.wav")
        };
        match tokio::fs::read_to_string(&meta).await {
            Ok(_) if age > SWEEP_GIVE_UP => {
                let _ = tokio::fs::rename(&wav, orphan("could not be registered for 7 days")).await;
                let _ = tokio::fs::rename(&meta, format!("{SPOOL}/{id}.orphan.meta")).await;
            }
            Ok(form) => match serde_urlencoded::from_str::<NewMessage>(form.trim()) {
                Ok(m) if m.id == id => {
                    tracing::info!("voicemail sweep: registering {id}");
                    if let Err(e) = register(app, m).await {
                        tracing::warn!("voicemail sweep {id}: {e:#}");
                    }
                }
                _ => tracing::warn!("voicemail sweep: {meta} unreadable, left in place"),
            },
            Err(_) => {
                let known: Option<i32> = sqlx::query_scalar("SELECT id FROM voicemails WHERE uuid = $1").bind(id).fetch_optional(&app.db).await?;
                if known.is_some() {
                    remove_spool(id).await;
                } else {
                    let _ = tokio::fs::rename(&wav, orphan("has no mailbox information")).await;
                }
            }
        }
    }
    Ok(())
}

// --- Web UI ----------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct VmRow {
    id: i32,
    received: String,
    caller_e164: Option<String>,
    caller_name: Option<String>,
    duration_s: Option<i32>,
    mailbox: String,
    heard: bool,
}

impl VmRow {
    fn caller(&self) -> String {
        match (&self.caller_name, &self.caller_e164) {
            (Some(n), Some(c)) => format!("{n} · {c}"),
            (None, Some(c)) => c.clone(),
            (Some(n), None) => n.clone(),
            (None, None) => "–".into(),
        }
    }
}

#[derive(Template)]
#[template(path = "voicemail.html")]
struct VoicemailPage {
    c: Ctx,
    messages: Vec<VmRow>,
    has_phone: bool,
    /// own greeting: (sound id, duration in seconds)
    greeting: Option<(i32, f64)>,
    max_greeting: u32,
    any_unheard: bool,
}

/// Messages a user may access: own mailbox and mailboxes of call groups they belong to.
pub(crate) const ACCESS: &str = "v.deleted_at IS NULL AND (
        (v.owner_type = 'user' AND v.owner_id = $1)
     OR (v.owner_type = 'group' AND v.owner_id IN (SELECT group_id FROM group_members WHERE user_id = $1 AND enabled)))";

pub async fn list(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let messages = sqlx::query_as(&format!(
        "SELECT v.id, to_char(v.received_at, 'DD.MM.YY HH24:MI') AS received,
                v.caller_e164, v.caller_name, v.duration_s,
                CASE v.owner_type WHEN 'user' THEN (SELECT extension::text FROM users WHERE id = v.owner_id)
                     ELSE (SELECT number || ' · ' || name FROM groups WHERE id = v.owner_id) END AS mailbox,
                v.heard_at IS NOT NULL AS heard
         FROM voicemails v WHERE {ACCESS} ORDER BY v.received_at DESC LIMIT 500"
    ))
    .bind(uid)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let greeting = own_greeting(&app.db, uid).await?;
    let any_unheard = messages.iter().any(|m: &VmRow| !m.heard);
    render(VoicemailPage { c: Ctx::new(&app, &u), messages, has_phone: true, greeting, max_greeting: MAX_GREETING_S, any_unheard })
}


pub(crate) async fn accessible(db: &PgPool, uid: i32, id: i32) -> Result<String, AppError> {
    sqlx::query_scalar(&format!("SELECT v.file_path FROM voicemails v WHERE v.id = $2 AND {ACCESS}"))
        .bind(uid)
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)
}

pub async fn audio(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let path = accessible(&app.db, uid, id).await?;
    let data = tokio::fs::read(&path).await.map_err(|_| AppError::NotFound)?;
    // Listening marks the message as heard
    // only the first time: the player may fetch the file several times
    let n = sqlx::query("UPDATE voicemails SET heard_at = now(), heard_by = $2 WHERE id = $1 AND heard_at IS NULL")
        .bind(id)
        .bind(uid)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if n > 0 {
        update_mwi_for(&app, id).await;
        changed(&app, id);
    }
    Ok(([(header::CONTENT_TYPE, "audio/mpeg"), (header::CACHE_CONTROL, "private, no-store")], Body::from(data)).into_response())
}

#[derive(Deserialize)]
pub struct VmAction {
    action: String,
}

pub async fn action(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, Form(f): Form<VmAction>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !matches!(f.action.as_str(), "heard" | "unheard" | "delete") {
        return Err(AppError::BadRequest("Unknown action.".into()));
    }
    if !apply(&app, uid, id, &f.action, &u.session.sub).await? {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/voicemail").into_response())
}

/// Marks every unheard message the user can access as heard (own and call group mailboxes).
pub async fn heard_all(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let ids: Vec<i32> = sqlx::query_scalar(&format!(
        "UPDATE voicemails v SET heard_at = now(), heard_by = $1 WHERE v.heard_at IS NULL AND {ACCESS} RETURNING v.id"
    ))
    .bind(uid)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    if !ids.is_empty() {
        db::audit(&app.db, &u.session.sub, "voicemail.heard_all", &format!("user:{uid}"), serde_json::json!({ "ids": ids })).await?;
        for id in ids {
            update_mwi_for(&app, id).await;
            changed(&app, id);
        }
    }
    Ok(Redirect::to("/voicemail").into_response())
}

/// heard / unheard / delete on behalf of a user (web UI and chat bot). false: no access.
pub async fn apply(app: &AppState, uid: i32, id: i32, action: &str, actor: &str) -> anyhow::Result<bool> {
    let path = match accessible(&app.db, uid, id).await {
        Ok(p) => p,
        Err(AppError::Internal(e)) => return Err(e),
        Err(_) => return Ok(false),
    };
    match action {
        "heard" => sqlx::query("UPDATE voicemails SET heard_at = now(), heard_by = $2 WHERE id = $1").bind(id).bind(uid).execute(&app.db).await?,
        "unheard" => sqlx::query("UPDATE voicemails SET heard_at = NULL, heard_by = NULL WHERE id = $1").bind(id).execute(&app.db).await?,
        "delete" => {
            remove_audio(&path).await;
            sqlx::query("UPDATE voicemails SET deleted_at = now() WHERE id = $1").bind(id).execute(&app.db).await?
        }
        _ => anyhow::bail!("unknown voicemail action"),
    };
    db::audit(&app.db, actor, &format!("voicemail.{action}"), &format!("voicemail:{id}"), serde_json::json!({})).await?;
    update_mwi_for(app, id).await;
    changed(app, id);
    Ok(true)
}

// --- Personal greeting -------------------------------------------------------------------

/// Longest accepted greeting in seconds
pub const MAX_GREETING_S: u32 = 60;

async fn own_greeting(db: &PgPool, uid: i32) -> Result<Option<(i32, f64)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT s.id, COALESCE(s.duration_s, 0)::float8 FROM greetings g JOIN sounds s ON s.id = g.sound_id AND s.deleted_at IS NULL
         WHERE g.owner_type = 'user' AND g.owner_id = $1 AND g.kind = 'unavailable'",
    )
    .bind(uid)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// Upload (file or browser recording) of the own greeting. Always normalised to WAV first, because
/// browser recordings (WebM/Ogg) often carry no duration in the container.
pub async fn greeting_upload(State(app): State<AppState>, u: CurrentUser, mut mp: axum::extract::Multipart) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let mut data = None;
    let mut fname = String::from("recording");
    while let Some(field) = mp.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        if field.name() == Some("file") {
            fname = field.file_name().unwrap_or("recording").chars().take(80).collect();
            data = Some(field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?);
        }
    }
    let data = data.filter(|d| !d.is_empty()).ok_or_else(|| AppError::BadRequest("No file.".into()))?;
    let tag = format!("greeting-{uid}-{}", std::process::id() as u64 ^ rand_suffix());
    let raw = std::env::temp_dir().join(format!("{tag}.in"));
    let wav = std::env::temp_dir().join(format!("{tag}.wav"));
    tokio::fs::write(&raw, &data).await.map_err(anyhow::Error::from)?;
    let ok = crate::sounds::run(
        Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(crate::sounds::SAFE_INPUT)
            .arg("-i")
            .arg(&raw)
            .args(["-vn", "-ac", "1", "-ar", "48000", "-t", &(MAX_GREETING_S + 1).to_string()])
            .arg(&wav),
    )
    .await
    .is_ok();
    let _ = tokio::fs::remove_file(&raw).await;
    if !ok {
        let _ = tokio::fs::remove_file(&wav).await;
        return Err(AppError::BadRequest("The file could not be read as audio.".into()));
    }
    let dur = duration(&wav).await.unwrap_or(0.0);
    if dur > MAX_GREETING_S as f64 + 0.5 || dur < 0.5 {
        let _ = tokio::fs::remove_file(&wav).await;
        return Err(AppError::BadRequest(format!("The greeting must be between 1 and {MAX_GREETING_S} seconds long.")));
    }
    let id: i32 = sqlx::query_scalar("INSERT INTO sounds (name, language, file_path, source_name, owner_user_id) VALUES ('Voicemail greeting', 'de', '', $1, $2) RETURNING id")
        .bind(&fname)
        .bind(uid)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let res = crate::sounds::convert(&wav, id).await;
    let _ = tokio::fs::remove_file(&wav).await;
    let dur = match res {
        Ok(d) => d,
        Err(e) => {
            sqlx::query("DELETE FROM sounds WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
            return Err(AppError::BadRequest(format!("The file could not be converted: {e}")));
        }
    };
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE sounds SET file_path = id::text, duration_s = $2 WHERE id = $1").bind(id).bind(dur).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    // the previous greeting is retired
    sqlx::query(
        "UPDATE sounds SET deleted_at = now() WHERE id IN (SELECT sound_id FROM greetings WHERE owner_type = 'user' AND owner_id = $1 AND kind = 'unavailable')",
    )
    .bind(uid)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO greetings (owner_type, owner_id, kind, sound_id) VALUES ('user', $1, 'unavailable', $2)
         ON CONFLICT (owner_type, owner_id, kind) DO UPDATE SET sound_id = EXCLUDED.sound_id",
    )
    .bind(uid)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "greeting.set", &format!("user:{uid}"), serde_json::json!({"sound": id, "duration": dur})).await?;
    Ok(Redirect::to("/voicemail").into_response())
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

pub async fn greeting_delete(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let sid: Option<i32> = sqlx::query_scalar("DELETE FROM greetings WHERE owner_type = 'user' AND owner_id = $1 AND kind = 'unavailable' RETURNING sound_id")
        .bind(uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(sid) = sid {
        sqlx::query("UPDATE sounds SET deleted_at = now() WHERE id = $1").bind(sid).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "greeting.delete", &format!("user:{uid}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/voicemail").into_response())
}

pub async fn greeting_audio(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let (sid, _) = own_greeting(&app.db, uid).await?.ok_or(AppError::NotFound)?;
    let data = tokio::fs::read(format!("{}/preview/{sid}.ogg", crate::sounds::MEDIA_DIR)).await.map_err(|_| AppError::NotFound)?;
    Ok(([(header::CONTENT_TYPE, "audio/ogg"), (header::CACHE_CONTROL, "private, no-cache")], Body::from(data)).into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn spool_entries() {
        assert_eq!(super::spool_entry("vm1795114218188412.wav"), Some(("vm1795114218188412", "wav")));
        assert_eq!(super::spool_entry("vm123.meta"), Some(("vm123", "meta")));
        assert_eq!(super::spool_entry("vm123.orphan.wav"), None);
        assert_eq!(super::spool_entry("vm.wav"), None);
        assert_eq!(super::spool_entry("x123.wav"), None);
    }

    #[test]
    fn phone_list_for_the_dialplan() {
        let m = |id, new, caller: Option<&str>| super::PhoneMsg {
            id,
            new,
            epoch: 1_800_000_000,
            caller: caller.map(String::from),
            file_path: format!("/media/voicemail/vm17{id}.mp3"),
        };
        let msgs = [m(7, true, Some("+4930123")), m(3, false, None), m(5, false, Some("+4415"))];
        assert_eq!(
            super::phone_list_text(1, 2, "Europe/Berlin", "49", &msgs),
            "ok,1,2,Europe/Berlin,7:1:1800000000:030123:vm177,3:0:1800000000::vm173,5:0:1800000000:004415:vm175"
        );
        assert_eq!(super::phone_list_text(0, 0, "UTC", "49", &[]), "ok,0,0,UTC");
        assert_eq!(super::phone_copy("/media/voicemail/vm1.mp3").to_str(), Some("/media/voicemail/vm1.wav"));
    }

    #[test]
    fn sidecar_as_written_by_the_dialplan() {
        // URIENCODE output of the hangup handler (sub-mailbox-done)
        let m: super::NewMessage = serde_urlencoded::from_str(
            "id=vm123&owner_type=user&owner=1010&caller=%2B4930123&name=M%C3%BCller%20(Firma)&did=%2B49301&call=1700000000.12",
        )
        .unwrap();
        assert_eq!((m.id.as_str(), m.owner.as_str(), m.caller.as_str()), ("vm123", "1010", "+4930123"));
        assert_eq!(m.name, "Müller (Firma)");
        let again: super::NewMessage = serde_urlencoded::from_str(&serde_urlencoded::to_string(&m).unwrap()).unwrap();
        assert_eq!(again.call, "1700000000.12");
    }
}
