// SPDX-License-Identifier: GPL-3.0-or-later
//! Daily housekeeping: retention of call records (cdr) and audit log (PBX_RETENTION_CALLS_DAYS,
//! PBX_RETENTION_AUDIT_DAYS; 0 = keep forever) and rotation of Asterisk's log files.
//! Voicemail retention is in voicemail.rs.

use std::time::Duration;

use anyhow::Result;

use crate::{AppState, ami};

pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            match crate::tts::cleanup(30).await {
                Ok(n) if n > 0 => tracing::info!("tts: {n} cached files unused for 30 days removed"),
                Err(e) => tracing::warn!("tts cleanup: {e:#}"),
                _ => {}
            }
            if let Err(e) = retention(&app).await {
                tracing::warn!("retention: {e:#}");
            }
            // messages.log / security.log: logger.conf keeps 7 rotated generations
            if let Err(e) = ami::command("logger rotate").await {
                tracing::warn!("Asterisk log rotation: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    });
}

async fn retention(app: &AppState) -> Result<()> {
    for (table, column, days) in [
        ("cdr", "calldate", app.cfg.retention_calls_days),
        ("call_flows", "started_at", app.cfg.retention_calls_days),
        ("audit_log", "at", app.cfg.retention_audit_days),
    ] {
        if days <= 0 {
            continue;
        }
        let n = sqlx::query(&format!("DELETE FROM {table} WHERE {column} < now() - make_interval(days => $1)"))
            .bind(days)
            .execute(&app.db)
            .await?
            .rows_affected();
        if n > 0 {
            tracing::info!("retention: {n} rows older than {days} days deleted from {table}");
        }
    }
    Ok(())
}
