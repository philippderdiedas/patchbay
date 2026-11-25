// SPDX-License-Identifier: GPL-3.0-or-later
//! In-process event bus for the business events (patchbay_notify::PbxEvent).
//!
//! Producers (call tracking, voicemail) call `publish`, which never blocks. One task adds names
//! from the database (persons, groups, IVRs, devices) and broadcasts the events in order;
//! consumers (bot, later webhooks) subscribe. Slow consumers lose events instead of blocking.

use std::sync::{Arc, Mutex};

use patchbay_notify::event::{Answerer, Party, PbxEvent, Presence, Target, TargetKind};
use sqlx::PgPool;
use tokio::sync::{broadcast, mpsc};

#[derive(Clone)]
pub struct Events {
    raw: mpsc::UnboundedSender<PbxEvent>,
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<PbxEvent>>>>,
    out: broadcast::Sender<Arc<PbxEvent>>,
}

impl Events {
    pub fn new() -> Self {
        let (raw, rx) = mpsc::unbounded_channel();
        let (out, _) = broadcast::channel(1024);
        Events { raw, rx: Arc::new(Mutex::new(Some(rx))), out }
    }

    pub fn publish(&self, ev: PbxEvent) {
        let _ = self.raw.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PbxEvent>> {
        self.out.subscribe()
    }

    /// Starts the enrichment task (once).
    pub fn spawn(&self, db: PgPool) {
        let Some(mut rx) = self.rx.lock().unwrap().take() else { return };
        let out = self.out.clone();
        tokio::spawn(async move {
            while let Some(mut ev) = rx.recv().await {
                if let Err(e) = enrich(&db, &mut ev).await {
                    tracing::warn!("event enrichment: {e:#}");
                }
                // calls are logged (one line per step, useful when a notification is missing);
                // presence changes are frequent
                if matches!(ev, PbxEvent::PresenceChanged { .. }) {
                    tracing::debug!("event {}", serde_json::to_string(&ev).unwrap_or_default());
                } else {
                    tracing::info!("event {}", serde_json::to_string(&ev).unwrap_or_default());
                }
                let _ = out.send(Arc::new(ev));
            }
        });
    }
}

async fn target_names(db: &PgPool, t: &mut Target) -> sqlx::Result<()> {
    if t.name.is_some() {
        return Ok(());
    }
    match t.kind {
        TargetKind::User => {
            t.name = sqlx::query_scalar("SELECT display_name FROM users WHERE extension::text = $1 AND deleted_at IS NULL").bind(&t.number).fetch_optional(db).await?;
        }
        TargetKind::Group => {
            t.name = sqlx::query_scalar("SELECT name FROM groups WHERE number::text = $1 AND deleted_at IS NULL").bind(&t.number).fetch_optional(db).await?;
        }
        TargetKind::Ivr => {
            // the dialplan knows the IVR by id; show its number if it has one
            let row: Option<(Option<String>, String)> = sqlx::query_as("SELECT number::text, name FROM ivrs WHERE id::text = $1").bind(&t.number).fetch_optional(db).await?;
            if let Some((num, name)) = row {
                if let Some(n) = num {
                    t.number = n;
                }
                t.name = Some(name);
            }
        }
    }
    Ok(())
}

async fn caller_name(db: &PgPool, p: &mut Party) -> sqlx::Result<()> {
    if p.name.is_none() && p.number.len() == 4 && p.number.chars().all(|c| c.is_ascii_digit()) {
        p.name = sqlx::query_scalar("SELECT display_name FROM users WHERE extension::text = $1 AND deleted_at IS NULL").bind(&p.number).fetch_optional(db).await?;
    }
    Ok(())
}

async fn answerer(db: &PgPool, a: &mut Answerer) -> sqlx::Result<()> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT label, extension::text, display_name FROM active_devices WHERE sip_username = $1").bind(&a.endpoint).fetch_optional(db).await?;
    if let Some((label, ext, name)) = row {
        a.device = label;
        a.user_ext = ext;
        a.user_name = name;
    }
    Ok(())
}

pub async fn enrich(db: &PgPool, ev: &mut PbxEvent) -> sqlx::Result<()> {
    match ev {
        PbxEvent::CallIncoming { caller, target, .. } => {
            caller_name(db, caller).await?;
            target_names(db, target).await?;
        }
        PbxEvent::CallAnswered { caller, target, by, .. } => {
            caller_name(db, caller).await?;
            if let Some(t) = target {
                target_names(db, t).await?;
            }
            answerer(db, by).await?;
        }
        PbxEvent::CallMissed { caller, target, rang, .. } => {
            caller_name(db, caller).await?;
            target_names(db, target).await?;
            // devices → persons ("Grace Schmidt (1010)"), each once
            let mut people: Vec<String> = vec![];
            for ep in rang.iter() {
                let mut a = Answerer { endpoint: ep.clone(), ..Default::default() };
                answerer(db, &mut a).await?;
                let l = a.label();
                if !people.contains(&l) {
                    people.push(l);
                }
            }
            *rang = people;
        }
        PbxEvent::CallEnded { caller, .. } => caller_name(db, caller).await?,
        PbxEvent::VoicemailNew { owner, caller, .. } => {
            caller_name(db, caller).await?;
            target_names(db, owner).await?;
        }
        // the bot reads the voicemail's current state itself
        PbxEvent::VoicemailChanged { .. } => {}
        PbxEvent::PresenceChanged { ext, state } => {
            if *state == Presence::Available {
                let dnd: Option<bool> = sqlx::query_scalar("SELECT dnd FROM users WHERE extension::text = $1 AND deleted_at IS NULL").bind(&*ext).fetch_optional(db).await?;
                *state = state.with_dnd(dnd == Some(true));
            }
        }
    }
    Ok(())
}
