// SPDX-License-Identifier: GPL-3.0-or-later
//! Mattermost adapter against a local mock of the REST API v4.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use patchbay_notify::{
    ChatPlatform, InboundHandler,
    mattermost::{Mattermost, MattermostConfig},
    model::{Action, Audio, Button, Dest, MessageRef, OutMessage, Reply, Section, Tone},
    platform::{Inbound, InboundKind},
};
use serde_json::{Value, json};

#[derive(Default)]
struct Mock {
    log: Mutex<Vec<(String, Value)>>,
}

type S = Arc<Mock>;

fn authed(h: &HeaderMap) -> Result<(), StatusCode> {
    (h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer bot-token")).then_some(()).ok_or(StatusCode::UNAUTHORIZED)
}

async fn me(h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    Ok(Json(json!({"id": "botid", "username": "pbx"})))
}

async fn user(h: HeaderMap, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    Ok(Json(json!({"id": id, "email": if id == "u1" { "a@example.com" } else { "" }})))
}

async fn direct(State(s): State<S>, h: HeaderMap, Json(b): Json<Value>) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    s.log.lock().unwrap().push(("direct".into(), b.clone()));
    Ok(Json(json!({"id": format!("dm-{}", b[1].as_str().unwrap())})))
}

async fn create_post(State(s): State<S>, h: HeaderMap, Json(b): Json<Value>) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    let mut log = s.log.lock().unwrap();
    log.push(("post".into(), b));
    Ok(Json(json!({"id": format!("post{}", log.len())})))
}

async fn patch(State(s): State<S>, h: HeaderMap, Path(id): Path<String>, Json(b): Json<Value>) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    s.log.lock().unwrap().push((format!("patch {id}"), b));
    Ok(Json(json!({"id": id})))
}

async fn files(State(s): State<S>, h: HeaderMap, mut mp: Multipart) -> Result<Json<Value>, StatusCode> {
    authed(&h)?;
    let mut info = json!({});
    while let Some(f) = mp.next_field().await.unwrap() {
        let name = f.name().unwrap().to_string();
        let fname = f.file_name().map(String::from);
        let ct = f.content_type().map(String::from);
        let data = f.bytes().await.unwrap();
        info[name] = json!({"file_name": fname, "content_type": ct, "len": data.len(), "text": String::from_utf8_lossy(&data)});
    }
    s.log.lock().unwrap().push(("files".into(), info));
    Ok(Json(json!({"file_infos": [{"id": "file1"}]})))
}

async fn mock() -> (String, S) {
    let s: S = Arc::new(Mock::default());
    let app = Router::new()
        .route("/api/v4/users/me", get(me))
        .route("/api/v4/users/{id}", get(user))
        .route("/api/v4/channels/direct", post(direct))
        .route("/api/v4/posts", post(create_post))
        .route("/api/v4/posts/{id}/patch", put(patch))
        .route("/api/v4/files", post(files))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), s)
}

fn adapter(url: &str) -> Arc<Mattermost> {
    Mattermost::new(MattermostConfig {
        url: url.into(),
        token: "bot-token".into(),
        slash_tokens: vec!["slash-a".into(), "slash-b".into()],
        action_url: "https://pbx.example.com/bot/mattermost/action".into(),
        action_key: b"0123456789abcdef0123456789abcdef".to_vec(),
    })
    .unwrap()
}

fn message() -> OutMessage {
    OutMessage::text("📞 Incoming call").section(Section {
        tone: Tone::Bad,
        fields: vec![("Dialled number".into(), "+4940".into())],
        buttons: vec![Button::new("Call back", Action::Call { n: "+4940123".into() }).primary()],
        ..Default::default()
    })
}

#[tokio::test]
async fn post_dm_edit_and_audio() {
    let (url, s) = mock().await;
    let mm = adapter(&url);
    let r = mm.send(&Dest::User("u1".into()), &message()).await.unwrap();
    assert_eq!(r, MessageRef { channel: "dm-u1".into(), id: "post2".into() });
    // DM channel is cached
    mm.send(&Dest::User("u1".into()), &OutMessage::text("again")).await.unwrap();
    mm.edit(&r, &OutMessage::text("✅ answered")).await.unwrap();
    let audio = Audio { data: b"ID3data".to_vec(), filename: "voicemail-7.mp3".into(), mime: "audio/mpeg".into(), duration_s: 3 };
    mm.send_audio(&Dest::Channel("chan".into()), &OutMessage::text("🎙 Voicemail"), &audio).await.unwrap();
    assert_eq!(mm.user_email("u1").await.unwrap(), Some("a@example.com".into()));
    assert_eq!(mm.user_email("u2").await.unwrap(), None);
    assert!(mm.user_email("../x").await.is_err());

    let log = s.log.lock().unwrap();
    let kinds: Vec<&str> = log.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, vec!["direct", "post", "post", "patch post2", "files", "post"]);
    assert_eq!(log[0].1, json!(["botid", "u1"]));
    let p = &log[1].1;
    assert_eq!(p["channel_id"], "dm-u1");
    assert_eq!(p["message"], "📞 Incoming call");
    let att = &p["props"]["attachments"][0];
    assert_eq!(att["color"], "#d24b4e");
    assert_eq!(att["fields"][0]["title"], "Dialled number");
    let act = &att["actions"][0];
    assert_eq!(act["name"], "Call back");
    assert_eq!(act["style"], "primary");
    assert_eq!(act["integration"]["url"], "https://pbx.example.com/bot/mattermost/action");
    // the context is signed and verifies
    assert_eq!(mm.verify(&act["integration"]["context"]), Some(Action::Call { n: "+4940123".into() }));
    assert_eq!(log[3].1["message"], "✅ answered");
    assert_eq!(log[4].1["channel_id"]["text"], "chan");
    assert_eq!(log[4].1["files"]["file_name"], "voicemail-7.mp3");
    assert_eq!(log[4].1["files"]["content_type"], "audio/mpeg");
    assert_eq!(log[5].1["file_ids"], json!(["file1"]));
}

#[tokio::test]
async fn api_errors_are_reported() {
    let (url, _) = mock().await;
    let mm = Mattermost::new(MattermostConfig {
        url,
        token: "wrong".into(),
        slash_tokens: vec![],
        action_url: String::new(),
        action_key: vec![1; 32],
    })
    .unwrap();
    let e = mm.send(&Dest::Channel("c".into()), &OutMessage::text("x")).await.unwrap_err();
    assert!(e.to_string().contains("401"), "{e}");
}

/// Records inbound messages and answers with a fixed reply.
#[derive(Default)]
struct Rec {
    got: Mutex<Vec<Inbound>>,
}

#[async_trait]
impl InboundHandler for Rec {
    async fn handle(&self, inbound: Inbound) -> Reply {
        self.got.lock().unwrap().push(inbound);
        Reply { message: Some(OutMessage::text("done")), update: Some(OutMessage::text("updated")) }
    }
}

#[tokio::test]
async fn slash_command_token_is_checked() {
    let mm = adapter("http://127.0.0.1:1");
    let rec = Rec::default();
    let mut form: HashMap<String, String> = [
        ("token", "slash-b"),
        ("user_id", "u1"),
        ("user_name", "anna"),
        ("channel_id", "c1"),
        ("channel_name", "town-square"),
        ("command", "/pbx"),
        ("text", "dnd on"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let (st, body) = mm.slash(&form, &rec).await;
    assert_eq!(st, 200);
    assert_eq!(body["response_type"], "ephemeral");
    assert_eq!(body["text"], "done");
    let got = rec.got.lock().unwrap().pop().unwrap();
    assert_eq!(got.kind, InboundKind::Command("dnd on".into()));
    assert_eq!((got.chat_user.as_str(), got.channel.as_str(), got.direct), ("u1", "c1", false));

    form.insert("token".into(), "slash-c".into());
    assert_eq!(mm.slash(&form, &rec).await.0, 401);
    form.remove("token");
    assert_eq!(mm.slash(&form, &rec).await.0, 401);
    assert!(rec.got.lock().unwrap().is_empty());
}

#[tokio::test]
async fn button_callbacks_must_be_signed() {
    let mm = adapter("http://127.0.0.1:1");
    let rec = Rec::default();
    let ctx = mm.attachments(&message())[0]["actions"][0]["integration"]["context"].clone();
    let body = json!({"user_id": "u1", "user_name": "anna", "channel_id": "c1", "channel_name": "a__b", "post_id": "p1", "context": ctx});
    let (st, resp) = mm.action(body.to_string().as_bytes(), &rec).await;
    assert_eq!(st, 200);
    assert_eq!(resp["update"]["message"], "updated");
    assert_eq!(resp["ephemeral_text"], "done");
    let got = rec.got.lock().unwrap().pop().unwrap();
    assert!(got.direct);
    assert_eq!(
        got.kind,
        InboundKind::Action { action: Action::Call { n: "+4940123".into() }, message: Some(MessageRef { channel: "c1".into(), id: "p1".into() }) }
    );

    // tampered action (other number, same signature)
    let mut forged = body.clone();
    forged["context"]["d"] = json!(r#"{"a":"call","n":"+19005551234"}"#);
    assert_eq!(mm.action(forged.to_string().as_bytes(), &rec).await.0, 401);
    // no context at all
    assert_eq!(mm.action(br#"{"user_id":"u1"}"#, &rec).await.0, 401);
    assert_eq!(mm.action(b"not json", &rec).await.0, 400);
    assert!(rec.got.lock().unwrap().is_empty());
}

#[test]
fn websocket_direct_messages() {
    let post = json!({"id": "p", "user_id": "u1", "channel_id": "dm1", "message": " who ", "type": ""});
    let ev = json!({"event": "posted", "data": {"channel_type": "D", "sender_name": "@anna", "post": post.to_string()}});
    let inb = Mattermost::parse_ws(&ev, "botid").unwrap();
    assert_eq!(inb.kind, InboundKind::Command("who".into()));
    assert_eq!((inb.chat_user.as_str(), inb.chat_user_name.as_str(), inb.channel.as_str(), inb.direct), ("u1", "anna", "dm1", true));
    // own posts, channel posts, system messages and other events are ignored
    assert!(Mattermost::parse_ws(&ev, "u1").is_none());
    let mut ch = ev.clone();
    ch["data"]["channel_type"] = json!("O");
    assert!(Mattermost::parse_ws(&ch, "botid").is_none());
    let sys = json!({"event": "posted", "data": {"channel_type": "D", "post": json!({"user_id": "u1", "channel_id": "d", "message": "x", "type": "system_join_channel"}).to_string()}});
    assert!(Mattermost::parse_ws(&sys, "botid").is_none());
    assert!(Mattermost::parse_ws(&json!({"event": "typing"}), "botid").is_none());
}
