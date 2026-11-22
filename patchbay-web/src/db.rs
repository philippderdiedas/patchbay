// SPDX-License-Identifier: GPL-3.0-or-later
//! Database access. All writes are recorded in the audit log.

use anyhow::{Result, anyhow};
use md5::{Digest, Md5};
use rand::{Rng, distributions::Alphanumeric};
use sha2::Sha256;
use sqlx::{FromRow, PgPool};

#[derive(FromRow, Clone, Debug)]
pub struct DeviceRow {
    pub id: i32,
    /// direct number (None when PBX_NUMBERS_DEVICE=off)
    pub number: Option<i16>,
    pub label: String,
    pub r#type: String,
    pub managed_by: String,
    pub transport: String,
    pub srtp: String,
    pub enabled: bool,
    pub ring: bool,
    pub ring_delay_s: i16,
    pub sip_username: String,
    #[sqlx(default)]
    pub admin_locked: bool,
    /// Live registration (filled from Asterisk, not stored)
    #[sqlx(skip)]
    pub reg: Option<crate::ami::Contact>,
}

impl DeviceRow {
    /// direct number as text ("" without one)
    pub fn num(&self) -> String {
        self.number.map(|n| n.to_string()).unwrap_or_default()
    }
    /// Ring delay choices for the device list: (seconds, selected)
    pub fn delay_options(&self) -> Vec<(i16, bool)> {
        [0, 5, 10, 15, 20, 30].into_iter().map(|s| (s, s == self.ring_delay_s)).collect()
    }
    /// "online" (registered and answering), "unreachable" (registered, qualify fails) or "offline"
    pub fn reg_state(&self) -> &'static str {
        match &self.reg {
            Some(c) if c.status == "Reachable" || c.status == "Unknown" || c.status == "Created" => "online",
            Some(_) => "unreachable",
            None => "offline",
        }
    }
    /// Details for the tooltip: address, round trip, user agent
    pub fn reg_detail(&self) -> String {
        match &self.reg {
            Some(c) => {
                let mut s = c.addr.clone();
                if let Some(r) = c.rtt_ms {
                    s += &format!(" · {r} ms");
                }
                // product/version only ("LinphoneAndroid/6.2.10 (Nothing Phone …) LinphoneSDK/…" → "LinphoneAndroid/6.2.10")
                let ua = c.user_agent.split(" (").next().unwrap_or("").trim();
                if !ua.is_empty() {
                    s += &format!(" · {ua}");
                }
                s
            }
            None => String::new(),
        }
    }
}

/// Adds the live registration state from Asterisk to the devices (best effort).
pub async fn with_registration(mut devices: Vec<DeviceRow>) -> Vec<DeviceRow> {
    if let Ok(contacts) = crate::ami::contacts().await {
        for d in &mut devices {
            d.reg = contacts.get(&d.sip_username).cloned();
        }
    }
    devices
}

#[derive(FromRow, Clone, Debug)]
pub struct UserRow {
    pub id: i32,
    pub kind: String,
    pub extension: i16,
    pub display_name: String,
    pub email: Option<String>,
    pub kc_subject: Option<String>,
    pub enabled: bool,
    pub phone_allowed: bool,
    pub in_phonebook: bool,
    pub devices: i64,
    /// login name of the local login, if any
    pub local_login: Option<String>,
}

/// Data for the Linphone configuration
#[derive(FromRow)]
pub struct ProvData {
    pub sip_username: String,
    pub ha1: String,
    pub display_name: String,
}

pub async fn audit(db: &PgPool, actor: &str, action: &str, object: &str, details: serde_json::Value) -> Result<()> {
    sqlx::query("INSERT INTO audit_log (actor, action, object, details) VALUES ($1, $2, $3, $4)")
        .bind(actor)
        .bind(action)
        .bind(object)
        .bind(details)
        .execute(db)
        .await?;
    Ok(())
}

/// Finds the person linked to a Keycloak account (by kc_subject only; accounts are linked
/// explicitly by an admin, never by e-mail). Refreshes the groups and, if given (Keycloak sync
/// mode), phone_allowed.
pub async fn link_user(db: &PgPool, sub: &str, email: Option<&str>, phone: Option<bool>, groups: &[String]) -> Result<Option<i32>> {
    let id: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE kc_subject = $1 AND deleted_at IS NULL")
        .bind(sub)
        .fetch_optional(db)
        .await?;
    if let Some(uid) = id {
        sqlx::query(
            "UPDATE users SET phone_allowed = COALESCE($2, phone_allowed), email = COALESCE($3, email), kc_groups = $4, updated_at = now() WHERE id = $1",
        )
            .bind(uid)
            .bind(phone)
            .bind(email)
            .bind(groups)
            .execute(db)
            .await?;
    }
    Ok(id)
}

pub async fn user_extension(db: &PgPool, user_id: i32) -> Result<i16> {
    Ok(sqlx::query_scalar("SELECT extension FROM users WHERE id = $1").bind(user_id).fetch_one(db).await?)
}

pub async fn devices_of(db: &PgPool, user_id: i32) -> Result<Vec<DeviceRow>> {
    Ok(sqlx::query_as(
        "SELECT id, number, label, type::text AS type, managed_by::text AS managed_by, transport::text AS transport, srtp,
                enabled, ring, ring_delay_s, sip_username, admin_locked
         FROM devices WHERE user_id = $1 AND deleted_at IS NULL ORDER BY number",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

pub async fn device_of(db: &PgPool, user_id: i32, device_id: i32) -> Result<Option<DeviceRow>> {
    Ok(sqlx::query_as(
        "SELECT id, number, label, type::text AS type, managed_by::text AS managed_by, transport::text AS transport, srtp,
                enabled, ring, ring_delay_s, sip_username, admin_locked
         FROM devices WHERE user_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .bind(device_id)
    .fetch_optional(db)
    .await?)
}

pub fn random_string(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

fn ha1(user: &str, realm: &str, pass: &str) -> String {
    hex::encode(Md5::digest(format!("{user}:{realm}:{pass}")))
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Creates a device and returns it together with the plaintext password
/// (shown only once, and not needed at all for Linphone).
pub async fn create_device(
    db: &PgPool,
    plan: &crate::numbers::NumberPlan,
    realm: &str,
    user_id: i32,
    label: &str,
    dtype: &str,
    managed_by: &str,
    transport: &str,
    srtp: &str,
) -> Result<(DeviceRow, String)> {
    let mut tx = db.begin().await?;
    sqlx::query_scalar::<_, i32>("SELECT id FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    // serialise number allocation: concurrent creates would otherwise pick the same free number
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('pbx.numbers.allocate'))").execute(&mut *tx).await?;
    let number = crate::numbers::allocate(&mut *tx, plan, "device").await.map_err(|e| anyhow!("{e:?}"))?;
    let user = hex::encode(rand::thread_rng().r#gen::<[u8; 6]>());
    let pass = random_string(24);
    let row: DeviceRow = sqlx::query_as(
        "INSERT INTO devices (user_id, number, label, type, managed_by, sip_username, ha1, transport, srtp)
         VALUES ($1, $2, $3, $4::device_type, $5::managed_by, $6, $7, $8::sip_transport, $9)
         RETURNING id, number, label, type::text AS type, managed_by::text AS managed_by, transport::text AS transport, srtp,
                   enabled, ring, ring_delay_s, sip_username, admin_locked",
    )
    .bind(user_id)
    .bind(number)
    .bind(label)
    .bind(dtype)
    .bind(managed_by)
    .bind(&user)
    .bind(ha1(&user, realm, &pass))
    .bind(transport)
    .bind(srtp)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((row, pass))
}

/// Creates a token; only its SHA-256 is stored.
pub async fn create_token(db: &PgPool, device_id: i32, kind: &str, valid_minutes: Option<i64>) -> Result<String> {
    let token = hex::encode(rand::thread_rng().r#gen::<[u8; 24]>());
    sqlx::query(
        "INSERT INTO provisioning_tokens (device_id, kind, token_sha256, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(mins => $4))",
    )
    .bind(device_id)
    .bind(kind)
    .bind(sha256_hex(&token))
    .bind(valid_minutes.map(|m| m as i32))
    .execute(db)
    .await?;
    Ok(token)
}

/// Redeems a (single-use) bootstrap token. Returns the device ID.
pub async fn consume_bootstrap(db: &PgPool, token: &str) -> Result<Option<i32>> {
    Ok(sqlx::query_scalar(
        "UPDATE provisioning_tokens SET used_at = now()
         WHERE token_sha256 = $1 AND kind = 'bootstrap' AND used_at IS NULL AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         RETURNING device_id",
    )
    .bind(sha256_hex(token))
    .fetch_optional(db)
    .await?)
}

pub async fn device_by_token(db: &PgPool, token: &str) -> Result<Option<i32>> {
    Ok(sqlx::query_scalar(
        "SELECT device_id FROM provisioning_tokens
         WHERE token_sha256 = $1 AND kind = 'device' AND revoked_at IS NULL",
    )
    .bind(sha256_hex(token))
    .fetch_optional(db)
    .await?)
}

pub async fn revoke_tokens(db: &PgPool, device_id: i32) -> Result<()> {
    sqlx::query("UPDATE provisioning_tokens SET revoked_at = now() WHERE device_id = $1 AND revoked_at IS NULL")
        .bind(device_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Only active Linphone devices get a configuration.
pub async fn prov_data(db: &PgPool, device_id: i32) -> Result<Option<ProvData>> {
    Ok(sqlx::query_as(
        "SELECT sip_username, ha1, display_name FROM active_devices
         WHERE id = $1 AND type = 'linphone'",
    )
    .bind(device_id)
    .fetch_optional(db)
    .await?)
}

pub async fn users(db: &PgPool) -> Result<Vec<UserRow>> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.kind::text AS kind, u.extension, u.display_name, u.email, u.kc_subject, u.enabled,
                u.phone_allowed, u.in_phonebook, count(d.id) AS devices,
                (SELECT login FROM local_logins WHERE user_id = u.id) AS local_login
         FROM users u LEFT JOIN devices d ON d.user_id = u.id AND d.deleted_at IS NULL
         WHERE u.deleted_at IS NULL GROUP BY u.id ORDER BY u.extension",
    )
    .fetch_all(db)
    .await?)
}

pub async fn user(db: &PgPool, user_id: i32) -> Result<Option<UserRow>> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.kind::text AS kind, u.extension, u.display_name, u.email, u.kc_subject, u.enabled,
                u.phone_allowed, u.in_phonebook, count(d.id) AS devices,
                (SELECT login FROM local_logins WHERE user_id = u.id) AS local_login
         FROM users u LEFT JOIN devices d ON d.user_id = u.id AND d.deleted_at IS NULL
         WHERE u.id = $1 AND u.deleted_at IS NULL GROUP BY u.id",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?)
}

pub async fn allows_sip(db: &PgPool, user_id: i32) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT allow_sip_devices FROM users WHERE id = $1").bind(user_id).fetch_one(db).await?)
}

/// Validated transport and SRTP mode from a form (defaults: TLS, SRTP if possible).
/// "required" only over TLS: SDES sends the SRTP keys in the SDP, over UDP/TCP in clear text, so
/// mandatory SRTP there only pretends security (stored as "optional").
pub fn transport_srtp<'a>(transport: &'a str, srtp: &'a str) -> (&'a str, &'a str) {
    let t = match transport {
        "udp" | "tcp" | "tls" => transport,
        _ => "tls",
    };
    let s = match srtp {
        "required" if t != "tls" => "optional",
        "required" | "optional" | "off" => srtp,
        _ => "optional",
    };
    (t, s)
}

#[cfg(test)]
mod transport_srtp_tests {
    #[test]
    fn srtp_required_only_over_tls() {
        assert_eq!(super::transport_srtp("tls", "required"), ("tls", "required"));
        assert_eq!(super::transport_srtp("udp", "required"), ("udp", "optional"));
        assert_eq!(super::transport_srtp("tcp", "required"), ("tcp", "optional"));
        assert_eq!(super::transport_srtp("udp", "off"), ("udp", "off"));
        assert_eq!(super::transport_srtp("x", "y"), ("tls", "optional"));
    }
}

/// Stored UI language of a user; sets it to `fallback` if none is stored yet.
pub async fn ui_language(db: &PgPool, user_id: i32, fallback: crate::i18n::Lang) -> Result<crate::i18n::Lang> {
    let l: Option<String> = sqlx::query_scalar(
        "UPDATE users SET ui_language = COALESCE(ui_language, $2) WHERE id = $1 RETURNING ui_language",
    )
    .bind(user_id)
    .bind(fallback.code())
    .fetch_one(db)
    .await?;
    Ok(l.as_deref().and_then(crate::i18n::Lang::parse).unwrap_or(fallback))
}
