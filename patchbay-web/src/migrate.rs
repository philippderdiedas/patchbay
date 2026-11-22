// SPDX-License-Identifier: GPL-3.0-or-later
//! `patchbay-web migrate`: applies the database migrations embedded at build time (db/migrations, see
//! build.rs) and sets the passwords of the login roles from secret files.
//!
//! Runs as the owner role `pbx` (DDL, CREATE ROLE), not as patchbay-web's restricted role `pbxweb`:
//!   PBX_MIGRATE_DATABASE_URL_FILE   file with postgres://pbx:<pw>@host:port/pbx
//!                                   (or PBX_MIGRATE_DATABASE_URL, for development)
//!   PBX_DB_PASSWORD_PBXWEB_FILE     password of role pbxweb   (optional, skipped when unset)
//!   PBX_DB_PASSWORD_ASTERISK_FILE   password of role asterisk (optional, skipped when unset)
//!
//! Bookkeeping: table schema_migrations(name, applied_at); files in
//! name order; each file and its row are committed in one transaction, so a failed migration
//! leaves no trace and is retried on the next run. A session advisory lock serialises concurrent
//! runners (two deploys, several Pods). Passwords are sent as SCRAM-SHA-256 verifiers: the plain
//! text never leaves this process (no command line, no statement log).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// pg_advisory_lock key ("pbx_migr")
const LOCK_KEY: i64 = 0x7062_785f_6d69_6772;
/// login roles whose password comes from a secret file
const ROLES: &[(&str, &str)] = &[("pbxweb", "PBX_DB_PASSWORD_PBXWEB_FILE"), ("asterisk", "PBX_DB_PASSWORD_ASTERISK_FILE")];
/// PostgreSQL's default iteration count for SCRAM-SHA-256
const SCRAM_ITERATIONS: u32 = 4096;
/// connection attempts (2 s apart): the database may still be starting
const CONNECT_ATTEMPTS: u32 = 30;

pub async fn cli_migrate() -> Result<()> {
    let url = match std::env::var("PBX_MIGRATE_DATABASE_URL") {
        Ok(u) => u,
        Err(_) => secret_file("PBX_MIGRATE_DATABASE_URL_FILE")?,
    };
    let mut conn = connect(&url).await?;
    sqlx::query("SELECT pg_advisory_lock($1)").bind(LOCK_KEY).execute(&mut conn).await?;
    let result = run(&mut conn).await;
    // the lock also ends with the session; unlock explicitly for a clean log on the server
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(LOCK_KEY).execute(&mut conn).await;
    let _ = conn.close().await;
    result
}

async fn connect(url: &str) -> Result<PgConnection> {
    let mut attempt = 1;
    loop {
        match PgConnection::connect(url).await {
            Ok(c) => return Ok(c),
            // the error text never contains the URL (password)
            Err(e) if attempt >= CONNECT_ATTEMPTS => return Err(anyhow!("database unreachable: {e}")),
            Err(e) => {
                tracing::warn!("database not reachable yet ({e}), retrying");
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn run(conn: &mut PgConnection) -> Result<()> {
    // created only when missing (no "already exists" notice)
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('schema_migrations') IS NOT NULL").fetch_one(&mut *conn).await?;
    if !exists {
        sqlx::raw_sql("CREATE TABLE schema_migrations (name text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())")
            .execute(&mut *conn)
            .await?;
    }
    let applied: HashSet<String> =
        sqlx::query_scalar("SELECT name FROM schema_migrations").fetch_all(&mut *conn).await?.into_iter().collect();

    // A database with migrations this image does not know comes from another line of versions (or a
    // newer image): applying this image's files on top would fail half-way or corrupt it.
    let known: HashSet<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
    let mut unknown: Vec<&String> = applied.iter().filter(|n| !known.contains(n.as_str())).collect();
    if !unknown.is_empty() {
        unknown.sort();
        anyhow::bail!(
            "the database has migrations this image does not contain: {unknown:?} – it belongs to another (newer?) version; refusing to migrate"
);
    }
    let mut pending: Vec<&(&str, &str)> = MIGRATIONS.iter().filter(|(n, _)| !applied.contains(*n)).collect();
    pending.sort_by_key(|(n, _)| *n);
    for (name, sql) in &pending {
        tracing::info!("migration: {name}");
        let mut tx = conn.begin().await?;
        // simple query protocol: a file may hold several statements (DO blocks, functions, …)
        sqlx::raw_sql(sql).execute(&mut *tx).await.with_context(|| format!("migration {name} failed"))?;
        sqlx::query("INSERT INTO schema_migrations (name) VALUES ($1)").bind(name).execute(&mut *tx).await?;
        tx.commit().await.with_context(|| format!("migration {name}: commit failed"))?;
    }
    tracing::info!("migrations: {} applied now, {} in total", pending.len(), MIGRATIONS.len());

    for (role, var) in ROLES {
        if std::env::var_os(var).is_none() {
            tracing::info!("{var} not set, password of role {role} left unchanged");
            continue;
        }
        let pw = secret_file(var)?;
        set_password(conn, role, &pw).await?;
    }
    Ok(())
}

/// Sets the role's password unless the stored verifier already matches it.
async fn set_password(conn: &mut PgConnection, role: &str, pw: &str) -> Result<()> {
    anyhow::ensure!(!pw.is_empty(), "password of role {role} is empty");
    // printable ASCII: PostgreSQL applies SASLprep to other characters, a client-side verifier could differ
    anyhow::ensure!(pw.bytes().all(|b| (0x20..0x7f).contains(&b)), "password of role {role}: only printable ASCII is supported");
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
        .bind(role)
        .fetch_one(&mut *conn)
        .await?;
    anyhow::ensure!(exists, "role {role} does not exist (the migrations create it)");
    // pg_authid is readable only by superusers; without it the password is simply set again
    let stored: Option<String> = sqlx::query_scalar("SELECT rolpassword FROM pg_authid WHERE rolname = $1")
        .bind(role)
        .fetch_optional(&mut *conn)
        .await
        .ok()
        .flatten()
        .flatten();
    if stored.as_deref().is_some_and(|s| scram_matches(s, pw)) {
        tracing::info!("password of role {role} unchanged");
        return Ok(());
    }
    let salt: [u8; 16] = rand::random();
    let verifier = scram_verifier(pw, &salt, SCRAM_ITERATIONS);
    // role names are constants, the verifier is base64 plus "$:" and digits: no quoting issues.
    // ALTER ROLE takes no bind parameters.
    debug_assert!(verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=$:-".contains(&b)));
    sqlx::raw_sql(&format!("ALTER ROLE {role} PASSWORD '{verifier}'"))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("setting the password of role {role} failed"))?;
    tracing::info!("password of role {role} set");
    Ok(())
}

/// Contents of the file named by the environment variable, trimmed.
fn secret_file(var: &str) -> Result<String> {
    let path = std::env::var(var).with_context(|| format!("environment variable {var} missing"))?;
    Ok(std::fs::read_to_string(&path).with_context(|| format!("{var}: {path} not readable"))?.trim().to_string())
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// PBKDF2-HMAC-SHA256 with one output block (32 bytes, what SCRAM-SHA-256 needs)
fn pbkdf2_sha256(pw: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(pw).expect("hmac accepts any key length");
    m.update(salt);
    m.update(&1u32.to_be_bytes());
    let mut u: [u8; 32] = m.finalize().into_bytes().into();
    let mut out = u;
    for _ in 1..iterations {
        u = hmac(pw, &u);
        out.iter_mut().zip(u.iter()).for_each(|(o, x)| *o ^= x);
    }
    out
}

/// PostgreSQL's stored form: SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>
fn scram_verifier(pw: &str, salt: &[u8], iterations: u32) -> String {
    let salted = pbkdf2_sha256(pw.as_bytes(), salt, iterations);
    let stored_key = Sha256::digest(hmac(&salted, b"Client Key"));
    let server_key = hmac(&salted, b"Server Key");
    format!("SCRAM-SHA-256${iterations}:{}${}:{}", B64.encode(salt), B64.encode(stored_key), B64.encode(server_key))
}

/// Whether a stored SCRAM verifier belongs to this password (false for md5 or anything unparsable)
fn scram_matches(stored: &str, pw: &str) -> bool {
    let parse = || -> Option<bool> {
        let rest = stored.strip_prefix("SCRAM-SHA-256$")?;
        let (params, _) = rest.split_once('$')?;
        let (iter, salt) = params.split_once(':')?;
        let iter: u32 = iter.parse().ok().filter(|i| (1..=1_000_000).contains(i))?;
        let salt = B64.decode(salt).ok()?;
        Some(scram_verifier(pw, &salt, iter) == stored)
    };
    parse().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_vectors() {
        // RFC 7914 §11 / well-known PBKDF2-HMAC-SHA256 vectors (P="password", S="salt")
        assert_eq!(hex::encode(pbkdf2_sha256(b"password", b"salt", 1)), "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b");
        assert_eq!(hex::encode(pbkdf2_sha256(b"password", b"salt", 2)), "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43");
        assert_eq!(hex::encode(pbkdf2_sha256(b"password", b"salt", 4096)), "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a");
    }

    #[test]
    fn scram_roundtrip() {
        let v = scram_verifier("secret", b"0123456789abcdef", 4096);
        assert!(v.starts_with("SCRAM-SHA-256$4096:MDEyMzQ1Njc4OWFiY2RlZg==$"));
        assert!(scram_matches(&v, "secret"));
        assert!(!scram_matches(&v, "Secret"));
        assert!(!scram_matches("md5abcdef", "secret"));
        assert!(!scram_matches("SCRAM-SHA-256$x:y$z", "secret"));
    }

    #[test]
    fn migrations_embedded_sorted_and_unique() {
        assert!(!MIGRATIONS.is_empty());
        assert!(MIGRATIONS.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(MIGRATIONS[0].0, "0001_initial.sql");
        assert!(MIGRATIONS.iter().all(|(n, sql)| n.ends_with(".sql") && !sql.trim().is_empty()));
    }
}
