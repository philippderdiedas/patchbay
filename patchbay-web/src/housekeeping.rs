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

/// every foreign key to targets (see the constraints on targets.id)
/// (only rows older than an hour: the UI writes a target just before the row that points to it)
const ORPHAN_TARGETS: &str = "DELETE FROM targets t WHERE t.created_at < now() - interval '1 hour'
   AND NOT EXISTS (SELECT 1 FROM users WHERE forward_immediate = t.id OR no_answer_target = t.id)
   AND NOT EXISTS (SELECT 1 FROM groups WHERE no_answer_target = t.id)
   AND NOT EXISTS (SELECT 1 FROM inbound_rules WHERE target_open = t.id OR target_closed = t.id OR target_holiday = t.id)
   AND NOT EXISTS (SELECT 1 FROM ivr_options WHERE target_id = t.id)
   AND NOT EXISTS (SELECT 1 FROM ivrs WHERE timeout_target = t.id)
   AND NOT EXISTS (SELECT 1 FROM trunk_inbound_rules WHERE target_id = t.id)";

async fn retention(app: &AppState) -> Result<()> {
    // webhook deliveries: sent ones after 30 days, failed ones after 90 (they show what went wrong)
    sqlx::query("DELETE FROM webhook_deliveries WHERE (status = 'sent' AND created_at < now() - interval '30 days') OR created_at < now() - interval '90 days'")
        .execute(&app.db)
        .await?;
    // bin of the shared phone book
    let n = sqlx::query("DELETE FROM contacts WHERE user_id IS NULL AND deleted_at < now() - make_interval(days => $1)")
        .bind(crate::contacts::BIN_DAYS)
        .execute(&app.db)
        .await?
        .rows_affected();
    if n > 0 {
        tracing::info!("retention: {n} shared contacts removed from the bin");
    }
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
    // call targets nothing points to any more (left behind by older versions, or a target changed
    // to "none"): they would still count as "used" in the delete checks of IVRs, groups, sounds.
    // Last and only logged on error: a target linked while this runs fails the delete (FK).
    match sqlx::query(ORPHAN_TARGETS).execute(&app.db).await {
        Ok(r) if r.rows_affected() > 0 => tracing::info!("retention: {} unused call targets removed", r.rows_affected()),
        Ok(_) => {}
        Err(e) => tracing::warn!("retention: unused call targets: {e}"),
    }
    Ok(())
}
