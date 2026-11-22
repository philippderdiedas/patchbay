// SPDX-License-Identifier: GPL-3.0-or-later
//! Linphone provisioning (docs/architecture.md, devices and provisioning).
//! /prov/b/<token>: single-use bootstrap token from the QR code, returns the configuration
//!                  including a permanent config-uri with a device token.
//! /prov/d/<token>: permanent device token so Linphone fetches updates on startup.

use axum::{
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::{AppError, AppState, db};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn entry(name: &str, value: &str) -> String {
    format!("    <entry name=\"{name}\" overwrite=\"true\">{}</entry>\n", xml_escape(value))
}

/// (mime, rate, channels, enabled) in order of preference
const AUDIO_CODECS: [(&str, &str, &str, bool); 8] = [
    ("opus", "48000", "2", true),
    ("G722", "8000", "1", true),
    ("PCMA", "8000", "1", true),
    ("PCMU", "8000", "1", false),
    ("speex", "16000", "1", false),
    ("speex", "8000", "1", false),
    ("G729", "8000", "1", false),
    ("GSM", "8000", "1", false),
];

pub fn linphone_xml(realm: &str, country_code: &str, p: &db::ProvData, config_uri: &str) -> String {
    let name = p.display_name.replace('"', "");
    let proxy = format!("<sip:{realm};transport=tls>");
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <config xmlns=\"http://www.linphone.org/xsds/lpconfig.xsd\">\n",
    );
    x += "  <section name=\"sip\">\n";
    x += &entry("media_encryption", "srtp");
    x += &entry("media_encryption_mandatory", "1");
    x += "  </section>\n  <section name=\"proxy_0\">\n";
    x += &entry("reg_proxy", &proxy);
    x += &entry("reg_route", &proxy);
    x += &entry("reg_identity", &format!("\"{name}\" <sip:{}@{realm}>", p.sip_username));
    x += &entry("realm", realm);
    x += &entry("reg_expires", "600");
    x += &entry("reg_sendregister", "1");
    x += &entry("publish", "0");
    x += &entry("dial_escape_plus", "0");
    // Own country code (PBX_COUNTRY_CODE), but numbers are sent unchanged; the PBX normalizes them itself.
    x += &entry("dial_prefix", country_code);
    x += &entry("use_dial_prefix_for_calls_and_chats", "0");
    // Voicemail: Linphone subscribes to message-summary on its own AOR (count of new messages) and
    // its voicemail button dials *97 (listen to the own and call group voicemail)
    x += &entry("mwi_server_uri", &format!("sip:{}@{realm};transport=tls", p.sip_username));
    x += &entry("voicemail_uri", &format!("sip:*97@{realm}"));
    x += "  </section>\n  <section name=\"auth_info_0\">\n";
    x += &entry("username", &p.sip_username);
    x += &entry("ha1", &p.ha1);
    x += &entry("realm", realm);
    x += &entry("domain", realm);
    x += &entry("algorithm", "MD5");
    // Contact search (as you type) against the PBX CardDAV server; Linphone answers its HTTP digest
    // challenge with auth_info_0, because the realm is the same as for SIP.
    x += "  </section>\n  <section name=\"remote_contact_directory_0\">\n";
    x += &entry("enabled", "1");
    x += &entry("type", "carddav");
    x += &entry("uri", &carddav_uri(config_uri));
    x += &entry("min_characters", "2");
    x += &entry("results_limit", "50");
    x += &entry("carddav_fields_for_user_input", "FN,N,ORG,TEL,EMAIL");
    x += &entry("carddav_fields_for_domain", "IMPP");
    // Audio codecs in the PBX's order: Opus first (mobile networks), then G.722 (HD, what the
    // desk phones use) and A-law; the rest off. Linphone matches by mime/rate/channels; G.722 is
    // registered with 8000 Hz (RFC 3551 quirk), Opus with 48000 Hz and 2 channels (RFC 7587).
    for (i, (mime, rate, ch, on)) in AUDIO_CODECS.iter().enumerate() {
        x += &format!("  </section>\n  <section name=\"audio_codec_{i}\">\n");
        x += &entry("mime", mime);
        x += &entry("rate", rate);
        x += &entry("channels", ch);
        x += &entry("enabled", if *on { "1" } else { "0" });
    }
    x += "  </section>\n  <section name=\"misc\">\n";
    x += &entry("config-uri", config_uri);
    x += "  </section>\n</config>\n";
    x
}

fn xml_response(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, "application/xml"), (header::CACHE_CONTROL, "no-store")],
        body,
    )
        .into_response()
}

fn valid_token(t: &str) -> bool {
    t.len() == 48 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn bootstrap(State(app): State<AppState>, Path(token): Path<String>) -> Result<Response, AppError> {
    if !valid_token(&token) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(device_id) = db::consume_bootstrap(&app.db, &token).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(p) = db::prov_data(&app.db, device_id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    // Invalidate old device tokens, then issue a new one
    db::revoke_tokens(&app.db, device_id).await?;
    let device_token = db::create_token(&app.db, device_id, "device", None).await?;
    db::audit(&app.db, "provisioning", "device.provision", &format!("device-id:{device_id}"), serde_json::json!({})).await?;
    let uri = format!("{}/prov/d/{device_token}", app.cfg.public_url);
    Ok(xml_response(linphone_xml(&app.cfg.sip_realm, &app.cfg.country_code, &p, &uri)))
}

pub async fn device(State(app): State<AppState>, Path(token): Path<String>) -> Result<Response, AppError> {
    if !valid_token(&token) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(device_id) = db::device_by_token(&app.db, &token).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(p) = db::prov_data(&app.db, device_id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let uri = format!("{}/prov/d/{token}", app.cfg.public_url);
    tracing::info!("prov: device {device_id} fetched its config");
    Ok(xml_response(linphone_xml(&app.cfg.sip_realm, &app.cfg.country_code, &p, &uri)))
}

/// https://host/carddav/book/ derived from the provisioning URL (same host as patchbay-web)
fn carddav_uri(config_uri: &str) -> String {
    let base = config_uri.split("/prov/").next().unwrap_or(config_uri).trim_end_matches('/');
    format!("{base}/carddav/book/")
}
