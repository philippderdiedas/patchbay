// SPDX-License-Identifier: GPL-3.0-or-later
//! patchbay-web: self-service and admin UI of the phone system, Linphone provisioning.

mod admin;
mod ami;
mod apps;
mod auth;
mod bot;
mod calls;
mod certwatch;
mod callflow;
mod calltrack;
mod carddav;
mod companion;
mod config;
mod contacts;
mod csv;
mod crypto;
mod custom;
mod db;
mod dupes;
mod events;
mod webhooks;
mod extbooks;
mod pages;
mod provision;
mod script;
mod secwatch;
mod tts;
mod routetest;
mod prov;
mod selfservice;
mod sounds;
mod sync;
mod times;
mod trunks;
mod voicemail;
mod help;
mod holidays;
mod blocklist;
mod notes;
mod flow;
mod monitor;
mod housekeeping;
mod hints;
mod moh;
mod numbers;
mod i18n;
mod local;
mod migrate;
mod proxy;
mod ui;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Router,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::Key;
use axum_server::tls_rustls::RustlsConfig;
use sqlx::postgres::PgPoolOptions;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<config::Config>,
    pub db: sqlx::PgPool,
    /// OIDC client, discovered at startup (PBX_AUTH=oidc) or on first use (both); empty with local
    pub oidc: Arc<tokio::sync::OnceCell<auth::Oidc>>,
    /// failed local logins (per login name and per client IP)
    pub login_limits: Arc<local::Limits>,
    pub cookie_key: Key,
    /// business events (call tracking, voicemail) for the chat bot and other outputs
    pub events: events::Events,
    pub bot: bot::BotState,
    /// call companion widget: "via" per call from call tracking, muted channels, channel cache
    pub live: companion::Live,
    /// webhook definitions (/config/webhooks.d), read at startup
    pub webhooks: webhooks::Shared,
}

impl AppState {
    /// Hangs up active calls of a locked device via AMI. Errors are only logged:
    /// through the views the device can no longer register and is unreachable anyway.
    pub async fn asterisk_kick(&self, sip_username: &str) {
        self.asterisk_refresh(sip_username).await;
        if let Err(e) = ami::hangup_endpoint(sip_username).await {
            tracing::warn!("AMI hangup for {sip_username} failed: {e:#}");
        }
    }

    /// Makes Asterisk re-read a device from the database (realtime cache).
    pub async fn asterisk_refresh(&self, sip_username: &str) {
        if let Err(e) = ami::expire_device(sip_username).await {
            tracing::warn!("AMI cache expiry for {sip_username} failed: {e:#}");
        }
    }
}

#[derive(Debug)]
pub enum AppError {
    Forbidden(String),
    BadRequest(String),
    NotFound,
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let logout = matches!(self, AppError::Forbidden(_));
        let (status, text) = match self {
            AppError::Forbidden(t) => (StatusCode::FORBIDDEN, t),
            AppError::BadRequest(t) => (StatusCode::BAD_REQUEST, t),
            AppError::NotFound => (StatusCode::NOT_FOUND, "Not found.".to_string()),
            AppError::Internal(e) => {
                tracing::error!("{e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error, see log.".to_string())
            }
        };
        let page = pages::MessagePage { c: ui::Ctx::anonymous(i18n::Lang::En), title: status.to_string(), text, logout };
        let body = askama::Template::render(&page).unwrap_or_else(|_| "Error".into());
        (status, Html(body)).into_response()
    }
}

const READY_FILE: &str = "/tmp/pbx-ready";

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    rustls::crypto::ring::default_provider().install_default().ok();

    // `patchbay-web mint-session [minutes] [user-id]`: prints a short-lived admin session cookie for browser
    // automation (UI tests without SSO). Needs only PBX_COOKIE_KEY_FILE; shows up in the audit log
    // as "automation:cli". With a user id the session acts as that phone user (self-service tests).
    if std::env::args().nth(1).as_deref() == Some("mint-session") {
        let minutes: i64 = std::env::args().nth(2).and_then(|m| m.parse().ok()).unwrap_or(60).clamp(1, 240);
        let key_hex = std::fs::read_to_string(std::env::var("PBX_COOKIE_KEY_FILE")?)?;
        let key = cookie::Key::from(&hex::decode(key_hex.trim())?);
        let user_id: Option<i32> = std::env::args().nth(3).and_then(|u| u.parse().ok());
        // acting as a person (user id given): also the phone group, so phone features (call
        // companion, contacts) can be tested; the person must still be active
        let mut names = vec![("PBX_GROUP_ADMIN", config::DEFAULT_GROUP_ADMIN), ("PBX_GROUP_WEBUI", config::DEFAULT_GROUP_WEBUI)];
        if user_id.is_some() {
            names.push(("PBX_GROUP_PHONE", config::DEFAULT_GROUP_PHONE));
        }
        let groups: Vec<String> = names.iter().map(|(v, d)| std::env::var(v).unwrap_or_else(|_| d.to_string())).collect();
        let session = auth::Session {
            sub: "automation:cli".into(),
            name: "Automation (CLI)".into(),
            email: None,
            groups,
            user_id,
            exp: chrono::Utc::now().timestamp() + minutes * 60,
            pwv: None,
        };
        let mut jar = cookie::CookieJar::new();
        jar.private_mut(&key).add(cookie::Cookie::new("pbx_session", serde_json::to_string(&session)?));
        println!("{}", jar.get("pbx_session").map(|c| c.value().to_string()).unwrap_or_default());
        return Ok(());
    }

    // `patchbay-web set-password <extension|login> [--login <name>] [--admin]`: creates or updates the
    // local login of an existing user, password from stdin (first admin without Keycloak).
    if std::env::args().nth(1).as_deref() == Some("set-password") {
        return local::cli_set_password(std::env::args().skip(2).collect()).await;
    }

    // `patchbay-web migrate`: applies the embedded database migrations as the owner role and sets the
    // login roles' passwords (migrate.rs). One-shot: run by patchbay-start before every start, or on its
    // own (e.g. as an initContainer/Job).
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return migrate::cli_migrate().await;
    }

    // `patchbay-web next-number <kind>`: prints the next free internal number of a kind per the number
    // plan (pbxctl uses it, so the allocation rules exist only once).
    if std::env::args().nth(1).as_deref() == Some("next-number") {
        return numbers::cli_next_number(std::env::args().nth(2)).await;
    }

    let cfg = config::Config::from_env()?;
    // every connection works in the site time zone (PBX_TIMEZONE): dates shown in the UI, current_date
    anyhow::ensure!(
        !cfg.timezone.is_empty() && cfg.timezone.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c)),
        "PBX_TIMEZONE={}: expected an IANA time zone like Europe/Berlin",
        cfg.timezone
    );
    let tz = cfg.timezone.clone();
    let db = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let tz = tz.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('TimeZone', $1, false)").bind(tz).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&cfg.database_url)
        .await
        .with_context(|| format!("Database unreachable (or PBX_TIMEZONE={} unknown to PostgreSQL)", cfg.timezone))?;
    let oidc = tokio::sync::OnceCell::new();
    if let Some(oc) = &cfg.oidc {
        match auth::Oidc::discover(oc, &cfg.public_url).await {
            Ok(o) => {
                let _ = oidc.set(o);
            }
            // only OIDC: no start without the identity provider
            Err(e) if !cfg.auth.local() => return Err(e),
            Err(e) => tracing::warn!("{e:#}; OIDC login is retried on first use, local login works"),
        }
    }
    tracing::info!("login: {:?}, Keycloak sync: {}", cfg.auth, cfg.kc_sync);
    let _ = ui::SITE_TITLE.set(cfg.site_title.clone());
    let state = AppState {
        cookie_key: Key::from(&cfg.cookie_key),
        cfg: Arc::new(cfg.clone()),
        db,
        oidc: Arc::new(oidc),
        login_limits: Arc::new(local::Limits::default()),
        events: events::Events::new(),
        bot: bot::BotState::default(),
        live: companion::Live::default(),
        webhooks: std::sync::Arc::new(webhooks::load(std::path::Path::new(&webhooks::dir()))),
    };

    // Site values the dialplan reads from the settings table (source of truth is the environment)
    // country code: also read by SQL (contact_name(), caller names in "My calls")
    admin::init_dialing(&cfg.country_code, &cfg.emergency_numbers);
    let recall = cfg.transfer_recall_s.to_string();
    let user_kinds = cfg.user_app_kinds.join(",");
    let shared_contacts = cfg.shared_contacts.as_str().to_string();
    for (k, v) in [
        // also the site id for loop detection in the IAX network (path of site ids, PBX_PATH)
        ("sip_realm", &cfg.sip_realm),
        ("country_code", &cfg.country_code),
        ("transfer_recall_s", &recall),
        // speaking clock (dialplan app "clock"): Asterisk itself runs in UTC
        ("timezone", &cfg.timezone),
        // app kinds users may use (PBX_USER_APPS); the app_dialplan view drops other user apps
        ("user_app_kinds", &user_kinds),
        // contact_name(): caller names from the shared phone book
        ("shared_contacts", &shared_contacts),
        // contact_name(): "Name (Company)" or the name only
        ("caller_name_format", &cfg.caller_name_format),
    ] {
        sqlx::query("INSERT INTO settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
            .bind(k)
            .bind(v)
            .execute(&state.db)
            .await
            .context("writing settings")?;
    }

    numbers::startup(&state.db, &cfg.numbers).await.context("number plan")?;

    if cfg.kc_sync {
        sync::spawn(state.clone());
    }
    times::spawn_holidays(state.db.clone(), &cfg);
    voicemail::spawn_cleanup(state.clone());
    // messages whose webhook call got lost (patchbay-web down while the caller hung up)
    voicemail::spawn_sweep(state.clone());
    // retention of call records and audit log, Asterisk log rotation
    housekeeping::spawn(state.clone());
    // webhooks: delivery loop and voicemail events from the event bus
    webhooks::spawn(state.clone());
    webhooks::spawn_bus(state.clone());
    // external phone books (PBX_EXTERNAL_BOOKS): periodic CardDAV loads
    extbooks::spawn(state.clone()).await.context("external phone books")?;
    {
        // message waiting lamps of all mailboxes (Asterisk keeps them only in memory)
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            voicemail::update_mwi(&st).await;
        });
    }
    custom::spawn(state.clone());
    trunks::spawn(state.clone());
    hints::spawn(state.clone());
    // call tracking (AMI) → events → chat bot
    state.events.spawn(state.db.clone());
    bot::start(&state).context("chat bot")?;
    calltrack::spawn(state.clone());
    {
        let st = state.clone();
        tokio::spawn(async move { moh::startup(&st).await });
    }
    // Devices are cached in Asterisk's memory (sorcery.conf); after migrations or a restore while
    // Asterisk kept running the cached objects may be stale: drop them once Asterisk answers.
    tokio::spawn(async {
        for attempt in 1..=30 {
            match ami::expire_device_caches().await {
                Ok(()) => {
                    tracing::info!("Asterisk device caches expired");
                    // "reject" markers of calls that ended while ringing (companion.rs, sub-user)
                    let _ = ami::action(&[("Action", "DBDelTree"), ("Family", "pbx_divert")]).await;
                    return;
                }
                Err(e) if attempt == 30 => tracing::warn!("expiring Asterisk device caches: {e:#}"),
                Err(_) => tokio::time::sleep(Duration::from_secs(10)).await,
            }
        }
    });
    if let Some(path) = cfg.asterisk_cert.clone() {
        certwatch::spawn(path, cfg.asterisk_restart_max_wait);
    }
    // Internal webhooks for Asterisk (plain HTTP, loopback only). The host network is shared with
    // other processes, so every call must carry the shared key (PBX_INTERNAL_KEY_FILE).
    let internal = voicemail::internal_router(state.clone())
        .merge(selfservice::internal_router(state.clone()))
        .merge(apps::internal_router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(state.clone(), internal_auth));
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind("127.0.0.1:8081").await {
            Ok(l) => {
                if let Err(e) = axum::serve(l, internal).await {
                    tracing::error!("internal listener: {e}");
                }
            }
            Err(e) => tracing::error!("internal listener bind failed: {e}"),
        }
    });

    let app = Router::new()
        .route("/", get(pages::index))
        .route("/phonebook", get(pages::phonebook))
        .route("/help", get(help::help))
        .route("/phonebook/status", get(pages::phonebook_status))
        .route("/calls", get(contacts::calls))
        .route("/calls.csv", get(contacts::calls_csv))
        .route("/me/dnd", post(selfservice::ui_dnd))
        .route("/me/claim", post(provision::claim))
        .route("/me/live", get(companion::live))
        .route("/me/live/context", get(companion::context))
        .route("/me/live/search", get(companion::search))
        .route("/me/live/book", get(companion::book))
        .route("/me/live/dnd", post(companion::dnd))
        .route("/me/live/block", post(blocklist::personal_widget))
        .route("/me/live/notes", post(notes::create))
        .route("/me/live/notes/{id}", post(notes::update))
        .route("/me/live/notes/{id}/delete", post(notes::delete))
        .route("/me/live/group/{gid}", post(companion::group))
        .route("/me/live/{id}/pickup", post(companion::pickup))
        .route("/me/live/{id}/{op}", post(companion::action))
        .route("/me/groups/{gid}/login", post(selfservice::ui_group_login))
        .route("/me/forward", post(selfservice::ui_forward))
        .route("/me/blocklist", post(blocklist::personal_create))
        .route("/me/blocklist/{id}/delete", post(blocklist::personal_delete))
        .route("/me/noanswer", post(selfservice::ui_noanswer))
        .route("/contacts/new", get(contacts::contact_new).post(contacts::contact_create))
        .route("/contacts/duplicates", get(dupes::page))
        .route("/contacts/duplicates/ignore", post(dupes::ignore))
        .route("/contacts/duplicates/unignore", post(dupes::unignore))
        .route("/contacts/merge", get(dupes::merge_page).post(dupes::merge))
        .route("/contacts/{id}", get(contacts::contact_edit).post(contacts::contact_save))
        .route("/contacts/{id}/delete", post(contacts::contact_delete))
        .route("/contacts/{id}/notes", post(contacts::contact_note_create))
        .route("/contacts/{id}/share", post(contacts::contact_share))
        .route("/contacts/{id}/copy", post(contacts::contact_copy))
        .route("/contacts/{id}/restore", post(contacts::contact_restore))
        .route("/contacts/{id}/notes/{note}/delete", post(contacts::contact_note_delete))
        .route("/apps", post(apps::create))
        .route("/apps/{id}", get(apps::edit).post(apps::save))
        .route("/apps/{id}/delete", post(apps::delete))
        .route("/apps/{id}/members", post(apps::member_add))
        .route("/apps/{id}/test", post(apps::script_test))
        .route("/apps/{id}/members/{uid}/delete", post(apps::member_remove))
        .route("/me/app-groups/{id}/leave", post(apps::leave))
        .route("/media", get(apps::media).post(apps::media_upload).layer(sounds::upload_limit()))
        .route("/media/{id}/preview", get(apps::media_preview))
        .route("/media/{id}/delete", post(apps::media_delete))
        .route("/media/hold-music", post(moh::personal_save))
        .route("/me/phonebook", post(pages::phonebook_visibility))
        .route("/me/number", post(pages::my_number))
        .route("/me/carddav", post(contacts::dav_token_create))
        .route("/me/carddav/{id}/delete", post(contacts::dav_token_delete))
        .route("/me/chat", get(bot::page))
        .route("/me/chat/code", post(bot::code_create))
        .route("/me/chat/unlink", post(bot::unlink))
        .route("/me/chat/delivery", post(bot::delivery))
        .route("/call", get(calls::call_page).post(calls::click_to_call))
        .route("/devices/{id}/ringtest", post(calls::ring_test))
        .route("/devices/{id}/provstatus", get(calls::prov_status))
        .route("/voicemail", get(voicemail::list))
        .route("/voicemail/greeting", post(voicemail::greeting_upload).layer(sounds::upload_limit()))
        .route("/voicemail/greeting/delete", post(voicemail::greeting_delete))
        .route("/voicemail/greeting/audio", get(voicemail::greeting_audio))
        .route("/voicemail/heard-all", post(voicemail::heard_all))
        .route("/voicemail/{id}/audio", get(voicemail::audio))
        .route("/voicemail/{id}", post(voicemail::action))
        .route("/devices", post(pages::device_create))
        .route("/devices/status", get(pages::devices_status))
        .route("/devices/{id}/qr", post(pages::device_qr))
        .route("/devices/{id}", post(pages::device_action))
        .route("/admin", get(pages::admin_users))
        .route("/admin/users", post(pages::admin_user_create))
        .route("/admin/users/sync", post(pages::admin_users_sync))
        .route("/admin/provision/preview", get(provision::preview))
        .route("/admin/users/{id}", get(pages::admin_user))
        .route("/admin/users/{id}/link", post(pages::admin_user_link))
        .route("/admin/users/{id}/unlink", post(pages::admin_user_unlink))
        .route("/admin/users/{id}/delete", post(admin::user_delete))
        .route("/admin/users/{id}/carddav", post(contacts::admin_dav_token_create))
        .route("/admin/users/{id}/edit", get(admin::user_edit).post(admin::user_save))
        .route("/admin/numbers", get(admin::numbers).post(admin::number_create))
        .route("/admin/numbers/{id}", get(times::number).post(admin::number_target))
        .route("/admin/flow/did/{id}", get(flow::did_page))
        .route("/admin/flow/ivr/{id}", get(flow::ivr_page))
        .route("/admin/flow/{number}", get(flow::internal_page))
        .route("/admin/classes", get(admin::classes).post(admin::class_save))
        .route("/admin/numberplan", get(numbers::plan_page))
        .route("/admin/numberplan/{number}/release", post(numbers::release))
        .route("/admin/classes/{key}/delete", post(admin::class_delete))
        .route("/admin/groups", get(admin::groups).post(admin::group_create))
        .route("/admin/groups/{id}", get(admin::group).post(admin::group_save))
        .route("/admin/groups/{id}/delete", post(admin::group_delete))
        .route("/admin/groups/{id}/members", post(admin::group_member_add))
        .route("/admin/groups/{id}/members/{uid}/delete", post(admin::group_member_remove))
        .route("/admin/groups/{id}/members/{uid}/login", post(admin::group_member_login))
        .route("/admin/sounds", get(sounds::sounds).post(sounds::sound_upload).layer(sounds::upload_limit()))
        .route("/admin/sounds/{id}/preview", get(sounds::sound_preview))
        .route("/admin/sounds/{id}", post(sounds::sound_rename))
        .route("/admin/sounds/{id}/delete", post(sounds::sound_delete))
        .route("/admin/moh", post(moh::create))
        .route("/admin/moh/{id}", get(moh::edit).post(moh::save))
        .route("/admin/moh/{id}/default", post(moh::make_default))
        .route("/admin/moh/{id}/delete", post(moh::delete))
        .route("/admin/ivrs", get(sounds::ivrs).post(sounds::ivr_create))
        .route("/admin/ivrs/{id}", get(sounds::ivr).post(sounds::ivr_save))
        .route("/admin/ivrs/{id}/options", post(sounds::ivr_option_set))
        .route("/admin/ivrs/{id}/delete", post(sounds::ivr_delete))
        .route("/admin/routes/new", get(admin::route_new).post(admin::route_create))
        .route("/admin/routes/{id}", get(admin::route_edit).post(admin::route_save))
        .route("/admin/routes/{id}/delete", post(admin::route_delete))
        .route("/admin/numbers/{id}/routing", post(times::number_save))
        .route("/admin/times", get(times::profiles).post(times::profile_create))
        .route("/admin/holidays", get(holidays::holidays).post(holidays::holiday_save))
        .route("/admin/holidays/import", post(holidays::import))
        .route("/admin/holidays/restore", post(holidays::holiday_restore))
        .route("/admin/holidays/sources/{id}/keep", post(holidays::source_keep))
        .route("/admin/holidays/sources/{id}/refresh", post(holidays::source_refresh))
        .route("/admin/holidays/sources/{id}/delete", post(holidays::source_delete))
        .route("/admin/holidays/{date}/delete", post(holidays::holiday_delete))
        .route("/admin/times/{id}", get(times::profile).post(times::profile_save))
        .route("/admin/times/{id}/delete", post(times::profile_delete))
        .route("/admin/times/{id}/rules", post(times::rule_add))
        .route("/admin/times/{id}/rules/{rid}/delete", post(times::rule_delete))
        .route("/admin/times/{id}/closures", post(times::closure_add))
        .route("/admin/times/{id}/closures/{date}/delete", post(times::closure_delete))
        .route("/admin/apps", get(custom::apps))
        .route("/admin/company-apps", post(apps::company_create))
        .route("/admin/apps/new", get(custom::app_new).post(custom::app_create))
        .route("/admin/apps/{id}", get(custom::app_edit).post(custom::app_save))
        .route("/admin/apps/{id}/delete", post(custom::app_delete))
        .route("/admin/trunks", get(trunks::trunks))
        .route("/admin/trunks/new", get(trunks::trunk_new).post(trunks::trunk_create))
        .route("/admin/trunks/{id}", get(trunks::trunk).post(trunks::trunk_save))
        .route("/admin/trunks/{id}/delete", post(trunks::trunk_delete))
        .route("/admin/trunks/{id}/test", post(trunks::trunk_test))
        .route("/admin/trunks/{id}/standard-route", post(trunks::standard_route))
        .route("/admin/trunks/{id}/rules", post(trunks::rule_add))
        .route("/admin/trunks/{id}/rules/{rid}/delete", post(trunks::rule_delete))
        .route("/admin/blocklist", get(blocklist::page).post(blocklist::add))
        .route("/admin/blocklist/anonymous", post(blocklist::anonymous))
        .route("/admin/blocklist/import", post(blocklist::import))
        .route("/admin/blocklist/{id}/delete", post(blocklist::delete))
        .route("/admin/status", get(admin::status))
        .route("/admin/log", get(admin::log))
        .route("/admin/webhooks", get(webhooks::admin_page))
        .route("/admin/webhooks/test", post(webhooks::admin_test))
        .route("/admin/webhooks/{id}/retry", post(webhooks::admin_retry))
        .route("/admin/log/calls.csv", get(admin::log_calls_csv))
        .route("/admin/users/{id}/devices", post(pages::admin_device_create))
        .route("/admin/users/{uid}/devices/{did}", post(pages::admin_device_action))
        .route("/auth/login", get(auth::login).post(local::login_submit))
        .route("/auth/oidc", get(auth::oidc_start))
        .route("/me/password", post(local::change_own))
        .route("/admin/users/{id}/login", post(local::admin_save))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/bye", get(pages::bye))
        .route("/prov/b/{token}", get(prov::bootstrap))
        .route("/prov/d/{token}", get(prov::device))
        .route("/static/base.css", get(ui::base_css))
        .route("/static/app.js", get(ui::app_js))
        .route("/static/logo", get(ui::logo))
        .route("/static/icon-32.png", get(ui::icon_32))
        .route("/static/icon-180.png", get(ui::icon_180))
        .route("/favicon.ico", get(ui::icon_32))
        .route("/static/theme.css", get(ui::theme_css))
        .route("/lang/{lang}", get(ui::set_lang))
        .route("/health", get(health))
        .route("/monitor", get(monitor::checkmk))
        .route("/monitor/{*key}", get(monitor::checkmk_one))
        .route("/metrics", get(monitor::prometheus))
        .route("/carddav", axum::routing::any(carddav::handle))
        .route("/carddav/", axum::routing::any(carddav::handle))
        .route("/carddav/{*rest}", axum::routing::any(carddav::handle))
        .route("/.well-known/carddav", axum::routing::any(carddav::handle))
        .layer(axum::middleware::from_fn_with_state(state.clone(), same_origin))
        // callbacks from the chat server: no browser origin, authenticated by token/signature
        .merge(bot::router())
        .with_state(state);

    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("PBX_LISTEN={}: expected address:port", cfg.listen))?;
    // As PID 1 in the container SIGTERM is ignored unless handled: stop on SIGTERM/SIGINT,
    // give open requests up to 3 s.
    let handle = axum_server::Handle::new();
    // readiness for the image's healthcheck (Asterisk waits for it in deploy/compose.yml): written
    // once the server listens, i.e. after migrations and startup
    let _ = std::fs::remove_file(READY_FILE);
    {
        let h = handle.clone();
        tokio::spawn(async move {
            if h.listening().await.is_some() {
                if let Err(e) = std::fs::write(READY_FILE, b"ok\n") {
                    tracing::warn!("{READY_FILE}: {e}");
                }
            }
        });
    }
    let h = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        h.graceful_shutdown(Some(Duration::from_secs(3)));
    });
    let svc = app.into_make_service_with_connect_info::<SocketAddr>();
    match &cfg.tls {
        Some((cert, key)) => {
            let tls = RustlsConfig::from_pem_file(cert, key).await?;
            // Reload the certificate hourly (renewed in the background by an ACME client)
            let reload = tls.clone();
            let (cert, key) = (cert.clone(), key.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    if let Err(e) = reload.reload_from_pem_file(&cert, &key).await {
                        tracing::warn!("Certificate reload failed: {e}");
                    }
                }
            });
            tracing::info!("patchbay-web {} listening on https://{addr}", config::build_version());
            axum_server::bind_rustls(addr, tls).handle(handle).serve(svc).await?;
        }
        None => {
            // PBX_TLS=off: a reverse proxy terminates HTTPS (PBX_PUBLIC_URL stays https://…)
            tracing::info!("patchbay-web {} listening on http://{addr} (PBX_TLS=off, behind a reverse proxy)", config::build_version());
            if cfg.trusted_proxies.is_empty() {
                tracing::warn!("PBX_TRUSTED_PROXIES is empty: login rate limits see the proxy's address, not the client's");
            }
            axum_server::bind(addr).handle(handle).serve(svc).await?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Internal webhooks: HTTP Basic `pbx:<internal key>`, compared in constant time.
async fn internal_auth(
    axum::extract::State(app): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let h = req.headers().get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !crypto::internal_auth_ok(h, &app.cfg.internal_key) {
        tracing::warn!("internal webhook {} without valid key", req.uri().path());
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

/// CSRF protection: state-changing requests must come from our own origin. Browsers send `Origin`
/// on every POST (and `Sec-Fetch-Site`); SameSite=Lax alone would still allow other hosts of the
/// same registrable domain (e.g. a compromised www.<domain>).
async fn same_origin(
    axum::extract::State(app): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::{http::Method, response::IntoResponse};
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let own = app.cfg.public_url.trim_end_matches('/');
    let h = req.headers();
    let ok = match h.get("origin").and_then(|v| v.to_str().ok()) {
        Some(o) => o.trim_end_matches('/') == own,
        None => matches!(h.get("sec-fetch-site").and_then(|v| v.to_str().ok()), None | Some("same-origin") | Some("none")),
    };
    if !ok {
        tracing::warn!("blocked cross-origin {} {}", req.method(), req.uri().path());
        return AppError::Forbidden("Request from another site blocked.".into()).into_response();
    }
    next.run(req).await
}
