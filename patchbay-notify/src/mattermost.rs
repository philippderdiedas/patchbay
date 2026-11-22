// SPDX-License-Identifier: GPL-3.0-or-later
//! Mattermost adapter: REST API v4 (posts, edits, files, DMs) with a bot account token,
//! WebSocket for direct messages to the bot, slash command `/pbx` and interactive buttons
//! (message attachments with integration actions) whose callbacks go to patchbay-web.
//!
//! Callbacks are authenticated: slash commands by their token (Mattermost sends it with every
//! request), button clicks by an HMAC over the action in the button's context (Mattermost strips
//! integration contexts from posts it sends to clients, the HMAC also prevents forged clicks).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::OnceCell;

use crate::{
    model::{Action, Audio, AudioFormat, ButtonStyle, Dest, MessageRef, OutMessage, Reply, Tone},
    platform::{Capabilities, ChatPlatform, Inbound, InboundHandler, InboundKind},
};

pub const ID: &str = "mattermost";

#[derive(Clone)]
pub struct MattermostConfig {
    /// server URL, e.g. https://mm.example.com
    pub url: String,
    /// access token of the bot account
    pub token: String,
    /// tokens of the `/pbx` slash commands (one per team); empty: slash commands refused
    pub slash_tokens: Vec<String>,
    /// URL Mattermost posts button clicks to (patchbay-web …/bot/mattermost/action)
    pub action_url: String,
    /// HMAC key for button contexts
    pub action_key: Vec<u8>,
}

pub struct Mattermost {
    cfg: MattermostConfig,
    http: reqwest::Client,
    me: OnceCell<String>,
    /// chat user id → DM channel id
    dm: Mutex<HashMap<String, String>>,
}

fn color(t: Tone) -> Option<&'static str> {
    match t {
        Tone::Neutral => None,
        Tone::Info => Some("#1c58d9"),
        Tone::Good => Some("#3db887"),
        Tone::Warn => Some("#ffbc1f"),
        Tone::Bad => Some("#d24b4e"),
    }
}

/// Constant-time comparison (slash tokens)
fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Mattermost {
    pub fn new(cfg: MattermostConfig) -> Result<Arc<Self>> {
        anyhow::ensure!(cfg.url.starts_with("https://") || cfg.url.starts_with("http://"), "Mattermost URL must start with https://");
        anyhow::ensure!(!cfg.token.is_empty(), "Mattermost bot token is empty");
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Arc::new(Mattermost { cfg, http, me: OnceCell::new(), dm: Mutex::new(HashMap::new()) }))
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api/v4{path}", self.cfg.url.trim_end_matches('/'))
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.bearer_auth(&self.cfg.token).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&body).ok().and_then(|v| v["message"].as_str().map(String::from)).unwrap_or(body);
            bail!("Mattermost API {status}: {}", msg.chars().take(300).collect::<String>());
        }
        Ok(if body.is_empty() { Value::Null } else { serde_json::from_str(&body)? })
    }

    /// user id of the bot account
    pub async fn me(&self) -> Result<String> {
        self.me
            .get_or_try_init(|| async {
                let v = self.call(self.http.get(self.api_url("/users/me"))).await?;
                v["id"].as_str().map(String::from).context("no user id in /users/me")
            })
            .await
            .cloned()
    }

    async fn channel(&self, dest: &Dest) -> Result<String> {
        match dest {
            Dest::Channel(c) => Ok(c.clone()),
            Dest::User(u) => {
                if let Some(c) = self.dm.lock().unwrap().get(u) {
                    return Ok(c.clone());
                }
                let me = self.me().await?;
                let v = self.call(self.http.post(self.api_url("/channels/direct")).json(&json!([me, u]))).await?;
                let id = v["id"].as_str().context("no channel id")?.to_string();
                self.dm.lock().unwrap().insert(u.clone(), id.clone());
                Ok(id)
            }
        }
    }

    fn sign(&self, action: &Action) -> Value {
        let data = serde_json::to_string(action).unwrap_or_default();
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.cfg.action_key).expect("hmac key");
        mac.update(data.as_bytes());
        json!({ "d": data, "s": hex::encode(mac.finalize().into_bytes()) })
    }

    /// Checks the signature of a button context and returns its action.
    pub fn verify(&self, ctx: &Value) -> Option<Action> {
        let data = ctx["d"].as_str()?;
        let sig = hex::decode(ctx["s"].as_str()?).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.cfg.action_key).ok()?;
        mac.update(data.as_bytes());
        mac.verify_slice(&sig).ok()?;
        serde_json::from_str(data).ok()
    }

    /// Message attachments (one per section) with buttons.
    pub fn attachments(&self, msg: &OutMessage) -> Vec<Value> {
        msg.sections
            .iter()
            .map(|s| {
                let mut a = json!({
                    "fallback": if s.text.is_empty() { msg.text.clone() } else { s.text.clone() },
                    "text": s.text,
                });
                if let Some(t) = &s.title {
                    a["title"] = json!(t);
                }
                if let Some(c) = color(s.tone) {
                    a["color"] = json!(c);
                }
                if !s.fields.is_empty() {
                    a["fields"] = s.fields.iter().map(|(k, v)| json!({"title": k, "value": v, "short": true})).collect();
                }
                if !s.buttons.is_empty() {
                    a["actions"] = s
                        .buttons
                        .iter()
                        .enumerate()
                        .map(|(i, b)| {
                            json!({
                                "id": format!("a{i}"),
                                "name": b.label,
                                "type": "button",
                                "style": match b.style { ButtonStyle::Default => "default", ButtonStyle::Primary => "primary", ButtonStyle::Danger => "danger" },
                                "integration": { "url": self.cfg.action_url, "context": self.sign(&b.action) },
                            })
                        })
                        .collect();
                }
                a
            })
            .collect()
    }

    fn post_body(&self, channel: &str, msg: &OutMessage) -> Value {
        json!({ "channel_id": channel, "message": msg.text, "props": { "attachments": self.attachments(msg) } })
    }

    // --- Callbacks (routes in patchbay-web) ------------------------------------------------------------

    /// Slash command `/pbx …` (application/x-www-form-urlencoded). Returns HTTP status and JSON.
    pub async fn slash(&self, form: &HashMap<String, String>, handler: &dyn InboundHandler) -> (u16, Value) {
        let token = form.get("token").map(String::as_str).unwrap_or("");
        if token.is_empty() || !self.cfg.slash_tokens.iter().any(|t| eq_ct(t.as_bytes(), token.as_bytes())) {
            tracing::warn!("mattermost: slash command with invalid token");
            return (401, json!({"text": "invalid token"}));
        }
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();
        let channel_name = get("channel_name");
        let inbound = Inbound {
            platform: ID,
            chat_user: get("user_id"),
            chat_user_name: get("user_name"),
            channel: get("channel_id"),
            direct: channel_name == "directmessage" || channel_name.contains("__"),
            kind: InboundKind::Command(get("text")),
        };
        if inbound.chat_user.is_empty() {
            return (400, json!({"text": "no user"}));
        }
        let reply = handler.handle(inbound).await;
        let msg = reply.message.or(reply.update).unwrap_or_else(|| OutMessage::text("✓"));
        (200, json!({ "response_type": "ephemeral", "text": msg.text, "attachments": self.attachments(&msg) }))
    }

    /// Button click (JSON body from Mattermost).
    pub async fn action(&self, body: &[u8], handler: &dyn InboundHandler) -> (u16, Value) {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            tracing::warn!("mattermost: button callback with invalid JSON ({} bytes)", body.len());
            return (400, json!({}));
        };
        tracing::info!(
            "mattermost: button callback user={} channel={} post={}",
            v["user_id"].as_str().unwrap_or("-"),
            v["channel_id"].as_str().unwrap_or("-"),
            v["post_id"].as_str().unwrap_or("-")
        );
        let Some(action) = self.verify(&v["context"]) else {
            tracing::warn!("mattermost: button callback with invalid signature");
            return (401, json!({}));
        };
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let channel_name = s("channel_name");
        let post = s("post_id");
        let inbound = Inbound {
            platform: ID,
            chat_user: s("user_id"),
            chat_user_name: s("user_name"),
            channel: s("channel_id"),
            direct: channel_name.contains("__"),
            kind: InboundKind::Action {
                action,
                message: (!post.is_empty()).then(|| MessageRef { channel: s("channel_id"), id: post }),
            },
        };
        if inbound.chat_user.is_empty() {
            tracing::warn!("mattermost: button callback without user_id");
            return (400, json!({}));
        }
        let reply = handler.handle(inbound).await;
        (200, self.action_response(&reply))
    }

    pub fn action_response(&self, reply: &Reply) -> Value {
        let mut out = json!({});
        if let Some(u) = &reply.update {
            out["update"] = json!({ "message": u.text, "props": { "attachments": self.attachments(u) } });
        }
        if let Some(m) = &reply.message {
            out["ephemeral_text"] = json!(m.plain());
        }
        out
    }

    /// A WebSocket event → inbound command, if it is a direct message to the bot from someone else.
    pub fn parse_ws(ev: &Value, me: &str) -> Option<Inbound> {
        if ev["event"] != "posted" || ev["data"]["channel_type"] != "D" {
            return None;
        }
        let post: Value = serde_json::from_str(ev["data"]["post"].as_str()?).ok()?;
        let user = post["user_id"].as_str()?;
        if user == me || !post["type"].as_str().unwrap_or("").is_empty() {
            return None;
        }
        let text = post["message"].as_str()?.trim();
        if text.is_empty() {
            return None;
        }
        Some(Inbound {
            platform: ID,
            chat_user: user.to_string(),
            chat_user_name: ev["data"]["sender_name"].as_str().unwrap_or("").trim_start_matches('@').to_string(),
            channel: post["channel_id"].as_str()?.to_string(),
            direct: true,
            kind: InboundKind::Command(text.to_string()),
        })
    }

    async fn ws_session(self: &Arc<Self>, handler: &Arc<dyn InboundHandler>) -> Result<()> {
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let me = self.me().await?;
        let url = format!("{}/api/v4/websocket", self.cfg.url.trim_end_matches('/')).replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let mut req = url.into_client_request()?;
        req.headers_mut().insert("Authorization", format!("Bearer {}", self.cfg.token).parse()?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req)).await.context("timeout")??;
        tracing::info!("mattermost: WebSocket connected");
        let (mut tx, mut rx) = ws.split();
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        let mut last = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if last.elapsed() > Duration::from_secs(95) {
                        bail!("no data for 95 s");
                    }
                    tx.send(Message::Ping(Vec::new().into())).await?;
                }
                m = rx.next() => {
                    let Some(m) = m else { bail!("closed") };
                    last = tokio::time::Instant::now();
                    let Message::Text(t) = m? else { continue };
                    let Ok(ev) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                    if let Some(inb) = Self::parse_ws(&ev, &me) {
                        // handled in its own task, so a slow PBX action does not stall the socket
                        let (this, h) = (self.clone(), handler.clone());
                        tokio::spawn(async move {
                            let channel = inb.channel.clone();
                            let reply = h.handle(inb).await;
                            if let Some(m) = reply.message.or(reply.update) {
                                if let Err(e) = this.send(&Dest::Channel(channel), &m).await {
                                    tracing::warn!("mattermost: reply failed: {e:#}");
                                }
                            }
                        });
                    }
                }
            }
        }
    }
}

#[async_trait]
impl ChatPlatform for Mattermost {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { buttons: true, edit: true, audio: Some(AudioFormat::Mp3), reactions: false, slash_commands: true, identity_email: true }
    }

    async fn send(&self, to: &Dest, msg: &OutMessage) -> Result<MessageRef> {
        let channel = self.channel(to).await?;
        let v = self.call(self.http.post(self.api_url("/posts")).json(&self.post_body(&channel, msg))).await?;
        Ok(MessageRef { channel, id: v["id"].as_str().context("no post id")?.to_string() })
    }

    async fn edit(&self, r: &MessageRef, msg: &OutMessage) -> Result<()> {
        let body = json!({ "message": msg.text, "props": { "attachments": self.attachments(msg) } });
        self.call(self.http.put(self.api_url(&format!("/posts/{}/patch", r.id))).json(&body)).await?;
        Ok(())
    }

    async fn send_audio(&self, to: &Dest, msg: &OutMessage, audio: &Audio) -> Result<MessageRef> {
        let channel = self.channel(to).await?;
        let part = reqwest::multipart::Part::bytes(audio.data.clone()).file_name(audio.filename.clone()).mime_str(&audio.mime)?;
        let form = reqwest::multipart::Form::new().text("channel_id", channel.clone()).part("files", part);
        let v = self.call(self.http.post(self.api_url("/files")).multipart(form)).await?;
        let file_id = v["file_infos"][0]["id"].as_str().context("no file id")?.to_string();
        let mut body = self.post_body(&channel, msg);
        body["file_ids"] = json!([file_id]);
        let v = self.call(self.http.post(self.api_url("/posts")).json(&body)).await?;
        Ok(MessageRef { channel, id: v["id"].as_str().context("no post id")?.to_string() })
    }

    async fn user_email(&self, chat_user: &str) -> Result<Option<String>> {
        anyhow::ensure!(chat_user.chars().all(|c| c.is_ascii_alphanumeric()), "invalid user id");
        let v = self.call(self.http.get(self.api_url(&format!("/users/{chat_user}")))).await?;
        Ok(v["email"].as_str().filter(|e| !e.is_empty()).map(String::from))
    }

    async fn run(self: Arc<Self>, handler: Arc<dyn InboundHandler>) {
        let mut backoff = 2;
        loop {
            let started = tokio::time::Instant::now();
            if let Err(e) = self.ws_session(&handler).await {
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = 2;
                }
                tracing::warn!("mattermost: WebSocket: {e:#}; reconnecting in {backoff} s");
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(120);
        }
    }
}
