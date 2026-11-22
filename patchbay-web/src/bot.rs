// SPDX-License-Identifier: GPL-3.0-or-later
//! Chat bot (crate patchbay-notify) inside patchbay-web: configuration from the environment, the PBX side
//! of the bot (`Pbx`/`Store` on top of the existing self-service functions), the Mattermost
//! callback routes and the personal chat settings page.

use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use askama::Template;
use patchbay_notify::async_trait;
use axum::{
    Form, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::post,
};
use patchbay_notify::{
    Bot, BotConfig, ChatPlatform, LinkMode,
    event::Presence,
    i18n::Lang,
    mattermost::{Mattermost, MattermostConfig},
    model::{Audio, AudioFormat},
    pbx::{CallListEntry, DirectoryEntry, Pbx, PbxGroup, PbxUser, Store, UserStatus, VmDelivery, VmOp, VoicemailInfo},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::{AppError, AppState, auth::CurrentUser, calltrack::presence_of, db, ui::{Ctx, render}};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Running bot, set once at startup when a chat platform is configured.
#[derive(Clone, Default)]
pub struct BotState(Arc<OnceLock<Running>>);

pub struct Running {
    pub bot: Arc<Bot>,
    pub mattermost: Option<Arc<Mattermost>>,
    pub link_mode: LinkMode,
}

impl BotState {
    pub fn enabled(&self) -> bool {
        self.0.get().is_some()
    }
    fn get(&self) -> Option<&Running> {
        self.0.get()
    }
}

impl Ctx {
    /// For templates: show the chat settings link
    pub fn chat_bot(&self) -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn secret_file(name: &str) -> Result<String> {
    let path = env(name).with_context(|| format!("{name} missing"))?;
    Ok(std::fs::read_to_string(&path).with_context(|| format!("{name}: {path} not readable"))?.trim().to_string())
}

/// Reads PBX_BOT_* and starts the bot. Without a configured platform the bot stays off.
pub fn start(app: &AppState) -> Result<()> {
    let link_mode = match env("PBX_BOT_LINK_MODE") {
        Some(m) => LinkMode::parse(&m).context("PBX_BOT_LINK_MODE must be auto or link")?,
        None => LinkMode::Code,
    };
    let lang = match env("PBX_BOT_LANGUAGE") {
        Some(l) => Lang::parse(&l).context("PBX_BOT_LANGUAGE must be en or de")?,
        None => Lang::En,
    };
    let callback = env("PBX_BOT_CALLBACK_URL").unwrap_or_else(|| app.cfg.public_url.clone());
    let mut platforms: Vec<Arc<dyn ChatPlatform>> = vec![];
    let mut mattermost = None;
    if let Some(url) = env("PBX_BOT_MATTERMOST_URL") {
        let token = secret_file("PBX_BOT_MATTERMOST_TOKEN_FILE")?;
        let slash_tokens = match env("PBX_BOT_MATTERMOST_SLASH_TOKEN_FILE") {
            Some(_) => secret_file("PBX_BOT_MATTERMOST_SLASH_TOKEN_FILE")?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
            None => {
                tracing::warn!("PBX_BOT_MATTERMOST_SLASH_TOKEN_FILE not set: /pbx slash commands are refused");
                vec![]
            }
        };
        // button contexts are signed with a key derived from the cookie key
        let action_key = Sha256::new().chain_update(b"pbx-bot-mattermost-actions").chain_update(&app.cfg.cookie_key).finalize().to_vec();
        let mm = Mattermost::new(MattermostConfig {
            url,
            token,
            slash_tokens,
            action_url: format!("{}/bot/mattermost/action", callback.trim_end_matches('/')),
            action_key,
        })?;
        platforms.push(mm.clone());
        mattermost = Some(mm);
    }
    if env("PBX_BOT_TELEGRAM_TOKEN_FILE").is_some() {
        tracing::warn!("PBX_BOT_TELEGRAM_TOKEN_FILE is set, but the Telegram adapter is not implemented yet");
    }
    if env("PBX_BOT_MATRIX_URL").is_some() {
        tracing::warn!("PBX_BOT_MATRIX_URL is set, but the Matrix adapter is not implemented yet");
    }
    if platforms.is_empty() {
        tracing::info!("chat bot off (no PBX_BOT_* platform configured)");
        return Ok(());
    }
    let bridge = Arc::new(Bridge { app: app.clone() });
    let bot = Bot::new(BotConfig { link_mode, lang, public_url: app.cfg.public_url.clone() }, bridge.clone(), bridge, platforms);
    if app.bot.0.set(Running { bot: bot.clone(), mattermost: mattermost.clone(), link_mode }).is_err() {
        anyhow::bail!("bot started twice");
    }
    ENABLED.store(true, Ordering::Relaxed);
    bot.spawn_inbound();
    if let Some(mm) = mattermost {
        tokio::spawn(async move {
            match mm.me().await {
                Ok(id) => tracing::info!("chat bot: Mattermost bot user {id}"),
                Err(e) => tracing::warn!("chat bot: Mattermost not reachable: {e:#}"),
            }
        });
    }
    // events in order, one at a time (a post must exist before it is edited)
    let mut rx = app.events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => bot.on_event(&ev).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!("chat bot: {n} events dropped"),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    tracing::info!("chat bot on ({link_mode:?} linking)");
    Ok(())
}

// --- Mattermost callbacks (outside the same-origin check; authenticated by token/HMAC) -------

pub fn router() -> Router<AppState> {
    Router::new().route("/bot/mattermost/command", post(mm_command)).route("/bot/mattermost/action", post(mm_action))
}

fn json(status: u16, body: serde_json::Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
}

async fn mm_command(State(app): State<AppState>, Form(form): Form<HashMap<String, String>>) -> Response {
    let Some(r) = app.bot.get() else { return StatusCode::NOT_FOUND.into_response() };
    let Some(mm) = &r.mattermost else { return StatusCode::NOT_FOUND.into_response() };
    let (st, body) = mm.slash(&form, r.bot.as_ref()).await;
    json(st, body)
}

async fn mm_action(State(app): State<AppState>, body: Bytes) -> Response {
    let Some(r) = app.bot.get() else { return StatusCode::NOT_FOUND.into_response() };
    let Some(mm) = &r.mattermost else { return StatusCode::NOT_FOUND.into_response() };
    let (st, body) = mm.action(&body, r.bot.as_ref()).await;
    json(st, body)
}

// --- PBX side of the bot ---------------------------------------------------------------------

struct Bridge {
    app: AppState,
}

fn actor(uid: i32) -> String {
    format!("bot:user-id:{uid}")
}

#[derive(sqlx::FromRow)]
struct VmRow {
    id: i32,
    received: String,
    caller_e164: Option<String>,
    caller_name: Option<String>,
    duration_s: Option<i32>,
    mailbox: String,
    heard: bool,
    deleted: bool,
}

impl From<VmRow> for VoicemailInfo {
    fn from(r: VmRow) -> Self {
        VoicemailInfo {
            id: r.id,
            received: r.received,
            caller_number: r.caller_e164,
            caller_name: r.caller_name,
            duration_s: r.duration_s.unwrap_or(0),
            mailbox: r.mailbox,
            heard: r.heard,
            deleted: r.deleted,
        }
    }
}

const VM_COLS: &str = "v.id, to_char(v.received_at, 'DD.MM.YY HH24:MI') AS received,
    v.caller_e164, v.caller_name, v.duration_s,
    CASE v.owner_type WHEN 'user' THEN (SELECT extension::text FROM users WHERE id = v.owner_id)
         ELSE (SELECT number || ' ' || name FROM groups WHERE id = v.owner_id) END AS mailbox,
    v.heard_at IS NOT NULL AS heard, v.deleted_at IS NOT NULL AS deleted";

/// Line state of the colleagues: extension → presence (hints + DND)
async fn presences(db: &PgPool) -> HashMap<String, Presence> {
    let states = crate::ami::extension_states().await.unwrap_or_default();
    let dnd: Vec<(String, bool)> = sqlx::query_as("SELECT extension::text, dnd FROM users WHERE deleted_at IS NULL").fetch_all(db).await.unwrap_or_default();
    dnd.into_iter()
        .map(|(ext, dnd)| {
            let p = presence_of(states.get(&ext).map(String::as_str).unwrap_or("Unavailable"));
            (ext, p.with_dnd(dnd))
        })
        .collect()
}

#[async_trait]
impl Pbx for Bridge {
    async fn user(&self, uid: i32) -> Result<Option<PbxUser>> {
        let row: Option<(i32, String, String, bool)> = sqlx::query_as(
            "SELECT id, extension::text, display_name, ($2 = ANY (kc_groups) OR web_admin) FROM users WHERE id = $1 AND deleted_at IS NULL AND enabled",
        )
        .bind(uid)
        .bind(&self.app.cfg.group_admin)
        .fetch_optional(&self.app.db)
        .await?;
        Ok(row.map(|(id, ext, name, admin)| PbxUser { id, ext, name, admin }))
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<PbxUser>> {
        let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1) AND deleted_at IS NULL AND enabled LIMIT 2")
            .bind(email.trim())
            .fetch_all(&self.app.db)
            .await?;
        // ambiguous addresses are never linked automatically
        match ids.as_slice() {
            [id] => self.user(*id).await,
            _ => Ok(None),
        }
    }

    async fn user_id_by_ext(&self, ext: &str) -> Result<Option<i32>> {
        Ok(sqlx::query_scalar("SELECT id FROM users WHERE extension::text = $1 AND deleted_at IS NULL").bind(ext).fetch_optional(&self.app.db).await?)
    }

    async fn group_by_number(&self, number: &str) -> Result<Option<PbxGroup>> {
        let row: Option<(i32, String, String)> = sqlx::query_as("SELECT id, number::text, name FROM groups WHERE number::text = $1 AND deleted_at IS NULL")
            .bind(number)
            .fetch_optional(&self.app.db)
            .await?;
        Ok(row.map(|(id, number, name)| PbxGroup { id, number, name }))
    }

    async fn group_label(&self, gid: i32) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT number || ' ' || name FROM groups WHERE id = $1 AND deleted_at IS NULL").bind(gid).fetch_optional(&self.app.db).await?)
    }

    async fn group_members(&self, gid: i32) -> Result<Vec<i32>> {
        Ok(sqlx::query_scalar("SELECT user_id FROM group_members WHERE group_id = $1 AND enabled").bind(gid).fetch_all(&self.app.db).await?)
    }

    async fn status(&self, uid: i32) -> Result<UserStatus> {
        let dnd: bool = sqlx::query_scalar("SELECT dnd FROM users WHERE id = $1").bind(uid).fetch_one(&self.app.db).await?;
        let na = crate::selfservice::noanswer_state(&self.app, uid).await?;
        Ok(UserStatus {
            dnd,
            forward: crate::selfservice::forward_text(&self.app, uid).await?,
            noanswer_mode: na.mode.to_string(),
            noanswer_value: na.value,
            ring_s: na.ring_s,
        })
    }

    async fn set_dnd(&self, uid: i32, on: bool) -> Result<bool> {
        crate::selfservice::set_dnd(&self.app, uid, Some(on)).await
    }

    async fn set_forward(&self, uid: i32, target: Option<&str>) -> Result<std::result::Result<(), String>> {
        use crate::selfservice::ForwardError;
        Ok(crate::selfservice::set_forward(&self.app, uid, target).await.map_err(|e| match e {
            ForwardError::Invalid(why) => why.to_string(),
            ForwardError::NotAllowed => "you may not call this number".to_string(),
        }))
    }

    async fn set_noanswer(&self, uid: i32, what: Option<patchbay_notify::pbx::NoAnswerChoice>, ring_s: Option<i16>) -> Result<std::result::Result<(), String>> {
        use crate::selfservice::{ForwardError, NoAnswer};
        use patchbay_notify::pbx::NoAnswerChoice as C;
        let what = what.map(|w| match w {
            C::Mailbox => NoAnswer::Mailbox,
            C::Number(n) => NoAnswer::Number(n),
            C::Nothing => NoAnswer::Nothing,
        });
        Ok(crate::selfservice::set_noanswer(&self.app, uid, what, ring_s).await.map_err(|e| match e {
            ForwardError::Invalid(why) => why.to_string(),
            ForwardError::NotAllowed => "you may not call this number".to_string(),
        }))
    }

    async fn originate(&self, uid: i32, to: &str) -> Result<std::result::Result<(), String>> {
        // Without a registered device the call would silently go nowhere
        let devices: Vec<String> = sqlx::query_scalar("SELECT sip_username FROM active_devices WHERE user_id = $1 AND ring").bind(uid).fetch_all(&self.app.db).await?;
        let contacts = crate::ami::contacts().await.unwrap_or_default();
        if !devices.iter().any(|d| contacts.contains_key(d)) {
            return Ok(Err("none of your phones is registered".into()));
        }
        Ok(crate::calls::originate_for(&self.app, uid, to, None, &actor(uid)).await?.map_err(String::from))
    }

    async fn voicemails(&self, uid: i32, limit: i64) -> Result<Vec<VoicemailInfo>> {
        let rows: Vec<VmRow> = sqlx::query_as(&format!("SELECT {VM_COLS} FROM voicemails v WHERE {} ORDER BY v.received_at DESC LIMIT $2", crate::voicemail::ACCESS))
            .bind(uid)
            .bind(limit)
            .fetch_all(&self.app.db)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn voicemail(&self, uid: Option<i32>, id: i32) -> Result<Option<VoicemailInfo>> {
        let row: Option<VmRow> = match uid {
            Some(uid) => sqlx::query_as(&format!("SELECT {VM_COLS} FROM voicemails v WHERE v.id = $2 AND {}", crate::voicemail::ACCESS))
                .bind(uid)
                .bind(id)
                .fetch_optional(&self.app.db)
                .await?,
            None => sqlx::query_as(&format!("SELECT {VM_COLS} FROM voicemails v WHERE v.id = $1")).bind(id).fetch_optional(&self.app.db).await?,
        };
        Ok(row.map(Into::into))
    }

    async fn voicemail_audio(&self, uid: Option<i32>, id: i32, format: AudioFormat) -> Result<Option<Audio>> {
        let path: Option<(String, Option<i32>)> = match uid {
            Some(uid) => sqlx::query_as(&format!("SELECT v.file_path, v.duration_s FROM voicemails v WHERE v.id = $2 AND {}", crate::voicemail::ACCESS))
                .bind(uid)
                .bind(id)
                .fetch_optional(&self.app.db)
                .await?,
            None => sqlx::query_as("SELECT file_path, duration_s FROM voicemails WHERE id = $1 AND deleted_at IS NULL").bind(id).fetch_optional(&self.app.db).await?,
        };
        let Some((path, dur)) = path else { return Ok(None) };
        let Ok(mp3) = tokio::fs::read(&path).await else { return Ok(None) };
        let duration_s = dur.unwrap_or(0);
        Ok(Some(match format {
            AudioFormat::Mp3 => Audio { data: mp3, filename: format!("voicemail-{id}.mp3"), mime: "audio/mpeg".into(), duration_s },
            AudioFormat::OggOpus => Audio { data: to_ogg_opus(&path).await?, filename: format!("voicemail-{id}.ogg"), mime: "audio/ogg".into(), duration_s },
        }))
    }

    async fn voicemail_op(&self, uid: i32, id: i32, op: VmOp) -> Result<bool> {
        let action = match op {
            VmOp::Heard => "heard",
            VmOp::Delete => "delete",
        };
        crate::voicemail::apply(&self.app, uid, id, action, &actor(uid)).await
    }

    async fn directory(&self, uid: i32, query: &str) -> Result<Vec<DirectoryEntry>> {
        let q = query.trim();
        let colleagues: Vec<(String, String)> = sqlx::query_as(
            "SELECT display_name, extension::text FROM users
             WHERE deleted_at IS NULL AND enabled AND (in_phonebook OR $2)
               AND (strpos(lower(display_name), lower($1)) > 0 OR extension::text = $1)
             ORDER BY extension LIMIT 20",
        )
        .bind(q)
        .bind(self.app.cfg.phonebook_enforced)
        .fetch_all(&self.app.db)
        .await?;
        let groups: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, number::text FROM groups WHERE deleted_at IS NULL AND (strpos(lower(name), lower($1)) > 0 OR number::text = $1) ORDER BY number LIMIT 10",
        )
        .bind(q)
        .fetch_all(&self.app.db)
        .await?;
        let contacts: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT name, organization, phone, mobile FROM contacts
             WHERE user_id = $1 AND (strpos(lower(name), lower($2)) > 0 OR strpos(lower(COALESCE(organization, '')), lower($2)) > 0)
             ORDER BY lower(name) LIMIT 20",
        )
        .bind(uid)
        .bind(q)
        .fetch_all(&self.app.db)
        .await?;
        let pres = if colleagues.is_empty() { HashMap::new() } else { presences(&self.app.db).await };
        let mut out: Vec<DirectoryEntry> = colleagues
            .into_iter()
            .map(|(name, number)| DirectoryEntry { presence: pres.get(&number).copied(), name, number, kind: "colleague" })
            .collect();
        out.extend(groups.into_iter().map(|(name, number)| DirectoryEntry { name, number, kind: "group", presence: None }));
        for (name, org, phone, mobile) in contacts {
            let label = match org.filter(|o| !o.is_empty()) {
                Some(o) => format!("{name} ({o})"),
                None => name,
            };
            for n in [phone, mobile].into_iter().flatten() {
                out.push(DirectoryEntry { name: label.clone(), number: n.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect(), kind: "contact", presence: None });
            }
        }
        Ok(out)
    }

    async fn presence(&self, query: &str) -> Result<Vec<DirectoryEntry>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT display_name, extension::text FROM users
             WHERE deleted_at IS NULL AND enabled AND (in_phonebook OR $2)
               AND ($1 = '' OR strpos(lower(display_name), lower($1)) > 0 OR extension::text = $1)
             ORDER BY extension",
        )
        .bind(query.trim())
        .bind(self.app.cfg.phonebook_enforced)
        .fetch_all(&self.app.db)
        .await?;
        let pres = presences(&self.app.db).await;
        Ok(rows.into_iter().map(|(name, number)| DirectoryEntry { presence: pres.get(&number).copied(), name, number, kind: "colleague" }).collect())
    }

    async fn call_list(&self, uid: i32, limit: i64) -> Result<Vec<CallListEntry>> {
        crate::contacts::recent_calls(&self.app.db, uid, limit).await
    }

    async fn vm_delivery(&self, uid: i32) -> Result<VmDelivery> {
        let d: Option<String> = sqlx::query_scalar("SELECT vm_delivery FROM bot_user_prefs WHERE user_id = $1").bind(uid).fetch_optional(&self.app.db).await?;
        Ok(d.as_deref().and_then(VmDelivery::parse).unwrap_or(VmDelivery::Mail))
    }
}

/// MP3 → OGG/Opus (voice messages on Telegram/Matrix)
async fn to_ogg_opus(mp3: &str) -> Result<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("vm-{}-{}.ogg", std::process::id(), rand::random::<u32>()));
    let res = crate::sounds::run(
        tokio::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(crate::sounds::SAFE_INPUT)
            .arg("-i")
            .arg(mp3)
            .args(["-vn", "-ac", "1", "-c:a", "libopus", "-b:a", "32k"])
            .arg(&out),
    )
    .await;
    let data = tokio::fs::read(&out).await;
    let _ = tokio::fs::remove_file(&out).await;
    res?;
    Ok(data?)
}

#[async_trait]
impl Store for Bridge {
    async fn linked_user(&self, platform: &str, chat_user: &str) -> Result<Option<i32>> {
        Ok(sqlx::query_scalar(
            "SELECT l.user_id FROM bot_links l JOIN users u ON u.id = l.user_id
             WHERE l.platform = $1 AND l.chat_user = $2 AND u.deleted_at IS NULL AND u.enabled",
        )
        .bind(platform)
        .bind(chat_user)
        .fetch_optional(&self.app.db)
        .await?)
    }

    async fn chat_user_of(&self, platform: &str, uid: i32) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT chat_user FROM bot_links WHERE platform = $1 AND user_id = $2").bind(platform).bind(uid).fetch_optional(&self.app.db).await?)
    }

    async fn link_user(&self, platform: &str, chat_user: &str, chat_name: &str, uid: i32, via: &str) -> Result<()> {
        let mut tx = self.app.db.begin().await?;
        // one chat account per platform and user, one user per chat account
        sqlx::query("DELETE FROM bot_links WHERE platform = $1 AND (chat_user = $2 OR user_id = $3)").bind(platform).bind(chat_user).bind(uid).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO bot_links (platform, chat_user, chat_name, user_id, via) VALUES ($1, $2, $3, $4, $5)")
            .bind(platform)
            .bind(chat_user)
            .bind(chat_name.chars().take(100).collect::<String>())
            .bind(uid)
            .bind(via)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        db::audit(&self.app.db, &actor(uid), "bot.link", &format!("user-id:{uid}"), serde_json::json!({"platform": platform, "chat_user": chat_user, "via": via})).await?;
        Ok(())
    }

    async fn unlink_user(&self, platform: &str, chat_user: &str) -> Result<bool> {
        let uid: Option<i32> =
            sqlx::query_scalar("DELETE FROM bot_links WHERE platform = $1 AND chat_user = $2 RETURNING user_id").bind(platform).bind(chat_user).fetch_optional(&self.app.db).await?;
        if let Some(uid) = uid {
            db::audit(&self.app.db, &actor(uid), "bot.unlink", &format!("user-id:{uid}"), serde_json::json!({"platform": platform})).await?;
        }
        Ok(uid.is_some())
    }

    async fn redeem_code(&self, code: &str) -> Result<Option<i32>> {
        Ok(sqlx::query_scalar("DELETE FROM bot_link_codes WHERE code_hash = $1 AND expires_at > now() RETURNING user_id")
            .bind(code_hash(code))
            .fetch_optional(&self.app.db)
            .await?)
    }

    async fn group_channels(&self, gid: i32) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT platform, channel FROM bot_group_channels WHERE group_id = $1").bind(gid).fetch_all(&self.app.db).await?)
    }

    async fn channel_groups(&self, platform: &str, channel: &str) -> Result<Vec<i32>> {
        Ok(sqlx::query_scalar(
            "SELECT c.group_id FROM bot_group_channels c JOIN groups g ON g.id = c.group_id
             WHERE c.platform = $1 AND c.channel = $2 AND g.deleted_at IS NULL ORDER BY g.number",
        )
        .bind(platform)
        .bind(channel)
        .fetch_all(&self.app.db)
        .await?)
    }

    async fn link_group(&self, platform: &str, channel: &str, gid: i32, by: i32) -> Result<bool> {
        let n = sqlx::query("INSERT INTO bot_group_channels (platform, channel, group_id, linked_by) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(platform)
            .bind(channel)
            .bind(gid)
            .bind(by)
            .execute(&self.app.db)
            .await?
            .rows_affected();
        db::audit(&self.app.db, &actor(by), "bot.link_group", &format!("group-id:{gid}"), serde_json::json!({"platform": platform, "channel": channel})).await?;
        Ok(n > 0)
    }

    async fn unlink_group(&self, platform: &str, channel: &str, gid: i32) -> Result<bool> {
        let n = sqlx::query("DELETE FROM bot_group_channels WHERE platform = $1 AND channel = $2 AND group_id = $3")
            .bind(platform)
            .bind(channel)
            .bind(gid)
            .execute(&self.app.db)
            .await?
            .rows_affected();
        if n > 0 {
            db::audit(&self.app.db, "bot", "bot.unlink_group", &format!("group-id:{gid}"), serde_json::json!({"platform": platform, "channel": channel})).await?;
        }
        Ok(n > 0)
    }
}

fn code_hash(code: &str) -> String {
    hex::encode(Sha256::digest(code.trim().to_uppercase().as_bytes()))
}

/// 8 characters without look-alikes (0/O, 1/I)
fn new_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..8).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

// --- Web UI: personal chat settings ---------------------------------------------------------------

#[derive(sqlx::FromRow)]
pub struct LinkRow {
    platform: String,
    chat_name: String,
    via: String,
    linked: String,
}

#[derive(Template)]
#[template(path = "chat.html")]
struct ChatPage {
    c: Ctx,
    links: Vec<LinkRow>,
    code: Option<String>,
    delivery: String,
    auto_link: bool,
    platforms: String,
    /// command reference from patchbay-notify (same list as the bot's "help"): (syntax, text, admin)
    commands: Vec<(&'static str, &'static str, bool)>,
}

fn uid_of(app: &AppState, u: &CurrentUser) -> Result<i32, AppError> {
    if !app.bot.enabled() {
        return Err(AppError::NotFound);
    }
    u.phone_uid()
}

async fn chat_page(app: &AppState, u: &CurrentUser, code: Option<String>) -> Result<Response, AppError> {
    let uid = uid_of(app, u)?;
    let links: Vec<LinkRow> = sqlx::query_as(
        "SELECT platform, chat_name, via, to_char(linked_at, 'DD.MM.YY HH24:MI') AS linked
         FROM bot_links WHERE user_id = $1 ORDER BY platform",
    )
    .bind(uid)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let delivery: Option<String> = sqlx::query_scalar("SELECT vm_delivery FROM bot_user_prefs WHERE user_id = $1").bind(uid).fetch_optional(&app.db).await.map_err(anyhow::Error::from)?;
    let r = app.bot.get().ok_or(AppError::NotFound)?;
    let page = ChatPage {
        c: Ctx::new(app, u),
        links,
        code,
        delivery: delivery.unwrap_or_else(|| "mail".into()),
        auto_link: r.link_mode == LinkMode::Auto,
        platforms: r.bot.platforms().iter().map(|p| p.id()).collect::<Vec<_>>().join(", "),
        commands: {
            let lang = patchbay_notify::i18n::Lang::parse(u.lang.code()).unwrap_or(patchbay_notify::i18n::Lang::En);
            patchbay_notify::commands::COMMANDS.iter().map(|c| (c.syntax, lang.t(c.text), c.admin)).collect()
        },
    };
    render(page)
}

pub async fn page(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    chat_page(&app, &u, None).await
}

pub async fn code_create(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = uid_of(&app, &u)?;
    let code = new_code();
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM bot_link_codes WHERE user_id = $1 OR expires_at < now()").bind(uid).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    sqlx::query("INSERT INTO bot_link_codes (code_hash, user_id, expires_at) VALUES ($1, $2, now() + interval '15 minutes')")
        .bind(code_hash(&code))
        .bind(uid)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    chat_page(&app, &u, Some(code)).await
}

#[derive(Deserialize)]
pub struct UnlinkForm {
    platform: String,
}

pub async fn unlink(State(app): State<AppState>, u: CurrentUser, Form(f): Form<UnlinkForm>) -> Result<Response, AppError> {
    let uid = uid_of(&app, &u)?;
    sqlx::query("DELETE FROM bot_links WHERE user_id = $1 AND platform = $2").bind(uid).bind(&f.platform).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "bot.unlink", &format!("user-id:{uid}"), serde_json::json!({"platform": f.platform})).await?;
    Ok(Redirect::to("/me/chat").into_response())
}

#[derive(Deserialize)]
pub struct DeliveryForm {
    delivery: String,
}

pub async fn delivery(State(app): State<AppState>, u: CurrentUser, Form(f): Form<DeliveryForm>) -> Result<Response, AppError> {
    let uid = uid_of(&app, &u)?;
    let d = VmDelivery::parse(&f.delivery).ok_or_else(|| AppError::BadRequest("Unknown delivery.".into()))?;
    sqlx::query("INSERT INTO bot_user_prefs (user_id, vm_delivery) VALUES ($1, $2) ON CONFLICT (user_id) DO UPDATE SET vm_delivery = EXCLUDED.vm_delivery")
        .bind(uid)
        .bind(d.as_str())
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Redirect::to("/me/chat").into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_codes() {
        let c = super::new_code();
        assert_eq!(c.len(), 8);
        assert!(c.chars().all(|ch| ch.is_ascii_uppercase() || ('2'..='9').contains(&ch)));
        assert!(!c.contains('O') && !c.contains('I'));
        // codes are case-insensitive and trimmed
        assert_eq!(super::code_hash(&c), super::code_hash(&format!(" {} ", c.to_lowercase())));
    }
}
