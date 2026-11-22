// SPDX-License-Identifier: GPL-3.0-or-later
//! Restarts Asterisk when its TLS certificate changes (PBX_ASTERISK_CERT, shared keys mount written by
//! an ACME side container or cert-manager). The PJSIP TLS transport loads the certificate only at
//! start (a transport reload would drop the TLS connections), so a new certificate needs a restart.
//!
//! `core restart when convenient` lets Asterisk wait until no call is active and then restart in
//! place (same container); after PBX_ASTERISK_RESTART_MAX_WAIT seconds it is restarted anyway.
//! Detection: the file's hash changes while patchbay-web runs, or – at patchbay-web start – the file is newer
//! than Asterisk's start time (a renewal while patchbay-web was down).

use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::ami;

const INTERVAL: Duration = Duration::from_secs(300);
/// clock tolerance when comparing the file time with Asterisk's start
const SLACK: Duration = Duration::from_secs(30);

pub fn spawn(path: String, max_wait: u64) {
    tokio::spawn(async move {
        // let Asterisk finish booting first
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut seen: Option<[u8; 32]> = None;
        // a restart was requested at this instant (waiting for calls to end)
        let mut requested: Option<SystemTime> = None;
        let mut forced = false;
        loop {
            match tokio::fs::read(&path).await {
                Err(e) => tracing::warn!("Asterisk certificate {path}: {e}"),
                Ok(pem) => {
                    let hash: [u8; 32] = Sha256::digest(&pem).into();
                    let changed = match seen {
                        Some(h) => h != hash,
                        None => newer_than_asterisk(&path).await,
                    };
                    seen = Some(hash);
                    if changed && requested.is_none() {
                        match ami::command("core restart when convenient").await {
                            Ok(_) => {
                                tracing::info!("Asterisk certificate {path} changed: Asterisk restarts as soon as no call is active");
                                requested = Some(SystemTime::now());
                                forced = false;
                            }
                            // no call active: Asterisk restarts at once and may close the connection
                            // before answering
                            Err(_) if restarted_just_now().await => {
                                tracing::info!("Asterisk certificate {path} changed: Asterisk restarted with it");
                            }
                            // retried on the next round
                            Err(e) => {
                                tracing::warn!("Asterisk certificate changed, restart request failed: {e:#}");
                                seen = None;
                            }
                        }
                    }
                }
            }
            if let Some(at) = requested {
                let waited = at.elapsed().unwrap_or_default();
                match ami::uptime_seconds().await {
                    // restarted since the request
                    Ok(up) if Duration::from_secs(up) < waited => {
                        tracing::info!("Asterisk restarted with the new certificate");
                        requested = None;
                    }
                    Ok(_) if max_wait > 0 && waited > Duration::from_secs(max_wait) && !forced => {
                        tracing::warn!("calls still active after {max_wait}s, restarting Asterisk anyway for the new certificate");
                        if let Err(e) = ami::command("core restart now").await {
                            tracing::warn!("Asterisk restart failed: {e:#}");
                        }
                        forced = true;
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Whether Asterisk is up again with an uptime of seconds (waits for it a little).
async fn restarted_just_now() -> bool {
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Ok(up) = ami::uptime_seconds().await {
            return up < 60;
        }
    }
    false
}

/// Whether the file was written after Asterisk started (false when unknown).
async fn newer_than_asterisk(path: &str) -> bool {
    let Ok(mtime) = tokio::fs::metadata(path).await.and_then(|m| m.modified()) else { return false };
    let Ok(up) = ami::uptime_seconds().await else { return false };
    let Some(started) = SystemTime::now().checked_sub(Duration::from_secs(up)) else { return false };
    mtime > started + SLACK
}
