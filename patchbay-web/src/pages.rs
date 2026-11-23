// SPDX-License-Identifier: GPL-3.0-or-later
//! Pages for staff (self-service) and admins.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use axum::http::HeaderMap;

use crate::{
    AppError, AppState,
    i18n::Lang,
    ui::{Ctx, render},
    auth::{Admin, CurrentUser},
    db::{self, DeviceRow, UserRow},
};

fn qr_svg(data: &str) -> Result<String, AppError> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(anyhow::Error::from)?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(260, 260)
        .quiet_zone(true)
        .build())
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    c: Ctx,
    /// app kinds for "create app" (apps::KINDS)
    app_kinds: Vec<&'static str>,
    may_phone: bool,
    user_apps: bool,
    may_sip: bool,
    /// may add a mobile phone as a device (PBX_MOBILE_DEVICES=users, or admin)
    may_mobile: bool,
    in_phonebook: bool,
    phonebook_optional: bool,
    apps: Vec<crate::apps::AppRow>,
    extension: Option<i16>,
    devices: Vec<DeviceRow>,
    dnd: bool,
    /// immediate forward: number (form value)
    forward: Option<String>,
    /// immediate forward for display: "1021 · Jane Doe", "+49176…"
    forward_label: String,
    /// PBX_NUMBERS_PERSON=…:choose: users pick their own number (range shown as hint)
    choose_number: Option<String>,
    /// PBX_NUMBERS_APP=…:choose: number field when creating an app
    choose_app_number: bool,
    /// login name of the own local login (password form)
    local_login: Option<String>,
    password_changed: bool,
    /// OIDC subject of an unlinked account (the admin links it with this)
    login_id: String,
    /// PBX_PROVISION=self and no person yet: range for "claim a number"
    claim_range: Option<String>,
    /// PBX_USER_NOANSWER: own ring time and no-answer target (None when not allowed)
    noanswer: Option<crate::selfservice::NoAnswerState>,
    /// own call groups (log in/out)
    groups: Vec<crate::selfservice::MyGroup>,
    /// call group apps the person is a member of: (app id, number, name, owner)
    app_groups: Vec<(i32, i16, String, String)>,
    /// own blocklist (numbers that hear busy when calling the person)
    blocked: Vec<crate::blocklist::PersonalEntry>,
}

impl IndexPage {
    /// Personal apps of a kind the site no longer allows (PBX_USER_APPS) do not work.
    fn kind_allowed(&self, kind: &str) -> bool {
        self.app_kinds.contains(&kind)
    }
}

#[derive(Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    password: Option<String>,
}

#[derive(Template)]
#[template(path = "provision.html")]
struct ProvisionPage {
    c: Ctx,
    device: DeviceRow,
    url: String,
    qr: String,
    back: String,
}

#[derive(Template)]
#[template(path = "credentials.html")]
struct CredentialsPage {
    c: Ctx,
    device: DeviceRow,
    realm: String,
    password: String,
    back: String,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct AdminUsersPage {
    c: Ctx,
    users: Vec<UserRow>,
    auth: crate::config::AuthUi,
    /// Result of "sync now": (checked, granted, revoked) or the error text
    synced: Option<Result<(usize, usize, usize), String>>,
}

#[derive(Template)]
#[template(path = "admin_user.html")]
struct AdminUserPage {
    c: Ctx,
    auth: crate::config::AuthUi,
    user: UserRow,
    devices: Vec<DeviceRow>,
}

#[derive(sqlx::FromRow)]
pub struct PhonebookEntry {
    extension: i16,
    display_name: String,
    devices: i64,
    dnd: bool,
    #[sqlx(skip)]
    presence: &'static str,
}

#[derive(Template)]
#[template(path = "phonebook.html")]
struct PhonebookPage {
    c: Ctx,
    entries: Vec<PhonebookEntry>,
    has_phone: bool,
    contacts: Vec<crate::contacts::Contact>,
    /// shared phone book (PBX_SHARED_CONTACTS), its bin (admins) and whether the viewer edits it
    shared_on: bool,
    shared: Vec<crate::contacts::Contact>,
    shared_edit: bool,
    bin: Vec<crate::contacts::Contact>,
    bin_days: i32,
    tokens: Vec<crate::contacts::DavToken>,
    dav_url: String,
    extension: String,
    /// "Services": emergency numbers, feature codes, call groups, voice menus, company apps
    services: Vec<crate::help::Service>,
}

pub async fn phonebook(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    // With an enforced phone book (company sites) the users' opt-out is ignored
    let entries = sqlx::query_as(
        "SELECT u.extension, u.display_name, count(a.id) AS devices, u.dnd
         FROM users u LEFT JOIN active_devices a ON a.user_id = u.id
         WHERE u.deleted_at IS NULL AND u.enabled AND (u.in_phonebook OR $1) GROUP BY u.id ORDER BY u.extension",
    )
    .bind(app.cfg.phonebook_enforced)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let mut entries: Vec<PhonebookEntry> = entries;
    let states = crate::ami::extension_states().await.unwrap_or_default();
    for e in &mut entries {
        e.presence = presence(states.get(&e.extension.to_string()).map(String::as_str), e.dnd);
    }
    let (contacts, shared) = match u.session.user_id {
        Some(uid) => (crate::contacts::contacts_of(&app, uid, u.may_phone).await?, crate::contacts::shared_contacts(&app, &u, uid).await?),
        None => (vec![], vec![]),
    };
    let bin = if u.is_admin && app.cfg.shared_contacts.on() { crate::contacts::shared_bin(&app).await? } else { vec![] };
    let (tokens, extension) = match u.session.user_id {
        Some(uid) => (crate::contacts::dav_tokens_of(&app.db, uid).await?, crate::contacts::dav_user(uid)),
        None => (vec![], String::new()),
    };
    let dav_url = format!("{}/carddav/", app.cfg.public_url.trim_end_matches('/'));
    let services = crate::help::services(&app, crate::i18n::Tr(u.lang)).await?;
    render(PhonebookPage {
        c: Ctx::new(&app, &u),
        entries,
        has_phone: u.session.user_id.is_some(),
        contacts,
        shared_on: crate::contacts::shared_access(&app, &u).0,
        shared,
        shared_edit: crate::contacts::shared_access(&app, &u).1,
        bin,
        bin_days: crate::contacts::BIN_DAYS,
        tokens,
        dav_url,
        extension,
        services,
    })
}

/// Simplified line state for the phone book: available, ringing, busy, dnd, offline.
fn presence(hint: Option<&str>, dnd: bool) -> &'static str {
    crate::calltrack::presence_of(hint.unwrap_or("Unavailable")).with_dnd(dnd).as_str()
}

/// Live states for the web phone book (polled by app.js): extension -> state
pub async fn phonebook_status(State(app): State<AppState>, _u: CurrentUser) -> Result<Response, AppError> {
    let rows: Vec<(i16, bool)> = sqlx::query_as("SELECT extension, dnd FROM users WHERE deleted_at IS NULL AND enabled AND (in_phonebook OR $1)")
        .bind(app.cfg.phonebook_enforced)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let states = crate::ami::extension_states().await.unwrap_or_default();
    let map: serde_json::Map<String, serde_json::Value> = rows
        .into_iter()
        .map(|(ext, dnd)| (ext.to_string(), serde_json::Value::from(presence(states.get(&ext.to_string()).map(String::as_str), dnd))))
        .collect();
    Ok(axum::Json(map).into_response())
}

#[derive(Template)]
#[template(path = "message.html")]
pub struct MessagePage {
    pub c: Ctx,
    pub title: String,
    pub text: String,
    /// "Log out" button (403: a logged-in session without permission would otherwise be a dead end)
    pub logout: bool,
}

pub async fn bye(headers: HeaderMap) -> Result<Response, AppError> {
    let c = Ctx::anonymous(Lang::from_headers(&headers));
    render(MessagePage {
        title: c.t.s("Logged out").into(),
        text: c.t.s("You have been logged out of the phone system.").into(),
        c,
        logout: false,
    })
}

pub async fn index(
    State(app): State<AppState>,
    u: CurrentUser,
    axum::extract::Query(q): axum::extract::Query<IndexQuery>,
) -> Result<Response, AppError> {
    let (extension, devices, may_sip, in_phonebook) = match u.session.user_id {
        Some(uid) => (
            Some(db::user_extension(&app.db, uid).await?),
            db::with_registration(db::devices_of(&app.db, uid).await?).await,
            db::allows_sip(&app.db, uid).await? || u.is_admin,
            sqlx::query_scalar("SELECT in_phonebook FROM users WHERE id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?,
        ),
        None => (None, vec![], false, false),
    };
    let (dnd, fwd) = match u.session.user_id {
        Some(uid) => (
            sqlx::query_scalar("SELECT dnd FROM users WHERE id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?,
            crate::selfservice::forward_target(&app, uid).await?,
        ),
        None => (false, None),
    };
    let forward_label = match &fwd {
        Some((n, Some(name))) => format!("{n} · {name}"),
        Some((n, None)) => n.clone(),
        None => String::new(),
    };
    render(IndexPage {
        app_kinds: crate::apps::user_kinds(&app.cfg),
        dnd,
        forward: fwd.map(|(n, _)| n),
        forward_label,
        c: Ctx::new(&app, &u),
        may_phone: u.may_phone,
        user_apps: app.cfg.user_apps,
        may_sip,
        may_mobile: u.is_admin || app.cfg.mobile_devices_self,
        in_phonebook,
        phonebook_optional: !app.cfg.phonebook_enforced,
        apps: match u.session.user_id { Some(uid) => crate::apps::apps_of(&app.db, uid).await?, None => vec![] },
        extension,
        devices,
        choose_number: { let p = app.cfg.numbers.policy("person"); p.choose().then(|| p.user_range_label()) },
        choose_app_number: app.cfg.numbers.policy("app").choose(),
        local_login: match u.session.user_id {
            Some(uid) => sqlx::query_scalar("SELECT login FROM local_logins WHERE user_id = $1")
                .bind(uid)
                .fetch_optional(&app.db)
                .await
                .map_err(anyhow::Error::from)?,
            None => None,
        },
        password_changed: q.password.as_deref() == Some("changed"),
        login_id: if u.session.user_id.is_none() { u.session.sub.clone() } else { String::new() },
        claim_range: (app.cfg.provision == crate::config::Provision::SelfService
            && u.session.user_id.is_none()
            && !u.session.is_local()
            && !u.session.sub.starts_with("automation:"))
        .then(|| app.cfg.numbers.policy("person").user_range_label()),
        noanswer: match u.session.user_id {
            Some(uid) if app.cfg.user_noanswer => Some(crate::selfservice::noanswer_state(&app, uid).await?),
            _ => None,
        },
        groups: match u.session.user_id {
            Some(uid) => crate::selfservice::my_groups(&app, uid).await?,
            None => vec![],
        },
        app_groups: match u.session.user_id {
            Some(uid) => crate::apps::memberships(&app.db, uid).await?,
            None => vec![],
        },
        blocked: match u.session.user_id {
            Some(uid) if u.may_phone => crate::blocklist::personal_list(&app, uid).await?,
            _ => vec![],
        },
    })
}

#[derive(Deserialize)]
pub struct MyNumber {
    number: String,
}

/// Users choose their own number (PBX_NUMBERS_PERSON=…:choose), at most every PBX_NUMBER_CHANGE_DAYS.
pub async fn my_number(State(app): State<AppState>, u: CurrentUser, Form(f): Form<MyNumber>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !app.cfg.numbers.policy("person").choose() {
        return Err(AppError::Forbidden("Numbers are assigned by the admins at this site.".into()));
    }
    let days = app.cfg.numbers.change_days;
    let next: Option<String> = sqlx::query_scalar(
        "SELECT to_char(number_changed_at + make_interval(days => $2), 'DD.MM.YYYY') FROM users
         WHERE id = $1 AND number_changed_at > now() - make_interval(days => $2)",
    )
    .bind(uid)
    .bind(days)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some(d) = next {
        return Err(AppError::BadRequest(format!("You can change your number again from {d}.")));
    }
    let old = db::user_extension(&app.db, uid).await?;
    let n = crate::numbers::check_self(&app.db, &app.cfg.numbers, "person", &f.number, Some(uid)).await?;
    if n == old {
        return Ok(Redirect::to("/").into_response());
    }
    sqlx::query("UPDATE users SET extension = $2, number_changed_at = now(), updated_at = now() WHERE id = $1")
        .bind(uid)
        .bind(n)
        .execute(&app.db)
        .await
        .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &u.session.sub, "user.number", &format!("user-id:{uid}"), serde_json::json!({"old": old, "new": n})).await?;
    // caller ID and mailbox are part of the cached endpoint
    for d in db::devices_of(&app.db, uid).await? {
        app.asterisk_refresh(&d.sip_username).await;
    }
    crate::voicemail::mailbox_moved(&app, old).await;
    Ok(Redirect::to("/").into_response())
}

#[derive(Deserialize)]
pub struct NewDevice {
    label: String,
    #[serde(default)]
    r#type: String,
    /// type "mobile": the mobile number
    #[serde(default)]
    mobile: String,
    #[serde(default)]
    transport: String,
    #[serde(default)]
    srtp: String,
}

/// SIP address of a device with a fixed address: sip:[user@]host[:port], host a name or IPv4.
fn fixed_contact(s: &str) -> Result<String, AppError> {
    let s = s.trim();
    let rest = s.strip_prefix("sip:").unwrap_or(s);
    let (user, hostport) = match rest.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (hostport, None),
    };
    let ok = user.is_none_or(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_alphanumeric() || "._~+-".contains(c)))
        && !host.is_empty()
        && host.len() <= 100
        && host.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
        && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
    if !ok {
        return Err(AppError::BadRequest("Fixed address: sip:user@host:port, e.g. sip:42@10.0.0.5:5070".into()));
    }
    Ok(format!("sip:{rest}"))
}

fn clean_label(s: &str) -> Result<String, AppError> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 40 {
        return Err(AppError::BadRequest("Label: 1–40 characters.".into()));
    }
    Ok(s.to_string())
}

async fn provision_page(app: &AppState, u: &CurrentUser, device: DeviceRow, back: &str) -> Result<Response, AppError> {
    let token = db::create_token(&app.db, device.id, "bootstrap", Some(15)).await?;
    let url = format!("{}/prov/b/{token}", app.cfg.public_url);
    render(ProvisionPage {
        c: Ctx::new(&app, &u),
        qr: qr_svg(&url)?,
        url,
        device,
        back: back.to_string(),
    })
}

/// Self-service: new Linphone device for one's own extension.
pub async fn device_create(
    State(app): State<AppState>,
    u: CurrentUser,
    Form(f): Form<NewDevice>,
) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !u.may_phone {
        return Err(AppError::Forbidden(format!("No phone permission (group {}).", app.cfg.group_phone)));
    }
    let label = clean_label(&f.label)?;
    // PBX_DEVICES_PER_USER (devices created by admins count, but admins themselves are not limited)
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM devices WHERE user_id = $1 AND deleted_at IS NULL")
        .bind(uid)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if count >= app.cfg.devices_per_user && !u.is_admin {
        return Err(AppError::BadRequest(format!("At most {} devices per user – delete an old one first.", app.cfg.devices_per_user)));
    }
    if f.r#type == "mobile" {
        if !(u.is_admin || app.cfg.mobile_devices_self) {
            return Err(AppError::Forbidden("Only an admin can add a mobile phone here.".into()));
        }
        let e164 = crate::selfservice::mobile_number(&app, uid, &f.mobile).await?;
        let device = db::create_mobile_device(&app.db, uid, &label, &e164, "self").await?;
        db::audit(&app.db, &u.session.sub, "device.add", &format!("device-id:{}", device.id), serde_json::json!({"type": "mobile"})).await?;
        return Ok(Redirect::to("/").into_response());
    }
    if f.r#type == "generic" {
        // Plain SIP device: only with permission; credentials are shown once
        if !(u.is_admin || db::allows_sip(&app.db, uid).await?) {
            return Err(AppError::Forbidden("You may only create Linphone devices.".into()));
        }
        let (tr, srtp) = db::transport_srtp(&f.transport, &f.srtp);
        let (device, pass) = db::create_device(&app.db, &app.cfg.numbers, &app.cfg.sip_realm, uid, &label, "generic", "self", tr, srtp).await?;
        db::audit(&app.db, &u.session.sub, "device.add", &format!("device:{}", device.num()), serde_json::json!({"type": "generic"})).await?;
        return render(CredentialsPage {
            c: Ctx::new(&app, &u),
            device,
            realm: app.cfg.sip_realm.clone(),
            password: pass,
            back: "/".into(),
        });
    }
    let (device, _pass) = db::create_device(&app.db, &app.cfg.numbers, &app.cfg.sip_realm, uid, &label, "linphone", "self", "tls", "required").await?;
    db::audit(&app.db, &u.session.sub, "device.add", &format!("device:{}", device.num()), serde_json::json!({"type": "linphone"})).await?;
    provision_page(&app, &u, device, "/").await
}

/// New QR code for one's own Linphone device (e.g. after reinstalling the app).
pub async fn device_qr(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let d = db::device_of(&app.db, uid, id).await?.ok_or(AppError::NotFound)?;
    if d.r#type != "linphone" || !d.enabled {
        return Err(AppError::BadRequest("Only for active Linphone devices.".into()));
    }
    provision_page(&app, &u, d, "/").await
}

#[derive(Deserialize)]
pub struct DeviceAction {
    action: String,
    #[serde(default)]
    delay: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    srtp: Option<String>,
    /// admin action "password_set": the new SIP password
    #[serde(default)]
    password: Option<String>,
}

/// Ringing on/off (all devices), lock/unlock and delete (self-managed only).
pub async fn device_action(
    State(app): State<AppState>,
    u: CurrentUser,
    Path(id): Path<i32>,
    Form(f): Form<DeviceAction>,
) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let d = db::device_of(&app.db, uid, id).await?.ok_or(AppError::NotFound)?;
    let own = d.managed_by == "self";
    // ring delay: also for admin-managed devices (it only changes how the user's own calls ring)
    if f.action == "delay" {
        let s: i16 = f.delay.parse().unwrap_or(0);
        if ![0, 5, 10, 15, 20, 30].contains(&s) {
            return Err(AppError::BadRequest("Ring delay: 0, 5, 10, 15, 20 or 30 seconds.".into()));
        }
        sqlx::query("UPDATE devices SET ring_delay_s = $2 WHERE id = $1").bind(id).bind(s).execute(&app.db).await.map_err(anyhow::Error::from)?;
        db::audit(&app.db, &u.session.sub, "device.delay", &format!("device:{}", d.num()), serde_json::json!({"seconds": s})).await?;
        return Ok(Redirect::to("/").into_response());
    }
    let sql = match f.action.as_str() {
        "ring_on" => "UPDATE devices SET ring = true WHERE id = $1",
        "ring_off" => "UPDATE devices SET ring = false WHERE id = $1",
        "enable" if own && !d.admin_locked => "UPDATE devices SET enabled = true WHERE id = $1",
        "disable" if own => "UPDATE devices SET enabled = false WHERE id = $1",
        "delete" if own => "UPDATE devices SET deleted_at = now(), enabled = false WHERE id = $1",
        _ => return Err(AppError::Forbidden("This action is not allowed for this device.".into())),
    };
    sqlx::query(sql).bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    if matches!(f.action.as_str(), "disable" | "delete") {
        db::revoke_tokens(&app.db, id).await?;
        app.asterisk_kick(&d.sip_username).await;
    } else {
        app.asterisk_refresh(&d.sip_username).await;
    }
    db::audit(&app.db, &u.session.sub, &format!("device.{}", f.action), &format!("device:{}", d.num()), serde_json::json!({})).await?;
    Ok(Redirect::to("/").into_response())
}

// --- Admin -------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UsersQuery {
    sync: Option<String>,
    #[serde(default)]
    checked: usize,
    #[serde(default)]
    granted: usize,
    #[serde(default)]
    revoked: usize,
    #[serde(default)]
    err: String,
}

pub async fn admin_users(State(app): State<AppState>, Admin(u): Admin, axum::extract::Query(q): axum::extract::Query<UsersQuery>) -> Result<Response, AppError> {
    let synced = match q.sync.as_deref() {
        Some("ok") => Some(Ok((q.checked, q.granted, q.revoked))),
        Some(_) => Some(Err(q.err.chars().filter(|c| !c.is_control()).take(300).collect())),
        None => None,
    };
    render(AdminUsersPage { c: Ctx::new(&app, &u).section("users"), users: db::users(&app.db).await?, auth: app.cfg.auth_ui(), synced })
}

/// "Sync now": the Keycloak sync job on demand (phone permission and groups of all linked users).
pub async fn admin_users_sync(State(app): State<AppState>, Admin(u): Admin) -> Result<Response, AppError> {
    let to = match crate::sync::run(&app).await {
        Ok(r) => {
            db::audit(&app.db, &u.session.sub, "keycloak.sync", "users", serde_json::json!({"checked": r.checked, "granted": r.granted, "revoked": r.revoked}))
                .await?;
            format!("/admin?sync=ok&checked={}&granted={}&revoked={}", r.checked, r.granted, r.revoked)
        }
        Err(e) => {
            tracing::warn!("keycloak sync (manual) failed, nothing changed: {e:#}");
            let enc: String = format!("{e:#}").bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
            format!("/admin?sync=err&err={enc}")
        }
    };
    Ok(Redirect::to(&to).into_response())
}

#[derive(Deserialize)]
pub struct NewUser {
    #[serde(default)]
    extension: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    keycloak: String,
    kind: String,
}

pub async fn admin_user_create(
    State(app): State<AppState>,
    Admin(u): Admin,
    Form(f): Form<NewUser>,
) -> Result<Response, AppError> {
    let extension = crate::numbers::from_form(&app.db, &app.cfg.numbers, "person", &f.extension, None).await?;
    let kind = if f.kind == "function" { "function" } else { "person" };
    // With the Keycloak sync, persons are linked to their Keycloak account right away (by its sub,
    // chosen by the admin). Other OIDC providers: optional, the admin enters the subject.
    let found = match f.keycloak.trim() {
        "" if kind == "person" && app.cfg.kc_sync => {
            return Err(AppError::BadRequest("Persons need a Keycloak account (username or e-mail).".into()));
        }
        "" => None,
        q => Some(find_account(&app, q).await?),
    };
    let name = match (f.display_name.trim(), &found) {
        ("", Some(k)) if !k.name.is_empty() => clean_label(&k.name)?,
        (n, _) => clean_label(n)?,
    };
    sqlx::query(
        "INSERT INTO users (kind, extension, display_name, email, kc_subject, route_classes)
         VALUES ($1::user_kind, $2, $3, $4, $5, ARRAY(SELECT key FROM route_classes WHERE default_on ORDER BY key))",
    )
        .bind(kind)
        .bind(extension)
        .bind(&name)
        .bind(found.as_ref().and_then(|k| k.email.clone()))
        .bind(found.as_ref().map(|k| k.sub.clone()))
        .execute(&app.db)
        .await
        .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &u.session.sub, "user.add", &format!("user:{extension}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to("/admin").into_response())
}

pub async fn admin_user(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let user = db::user(&app.db, id).await?.ok_or(AppError::NotFound)?;
    render(AdminUserPage {
        auth: app.cfg.auth_ui(),
        c: Ctx::new(&app, &u).section("users"),
        devices: db::with_registration(db::devices_of(&app.db, id).await?).await,
        user,
    })
}

#[derive(Deserialize)]
pub struct AdminNewDevice {
    label: String,
    r#type: String,
    #[serde(default)]
    transport: String,
    #[serde(default)]
    srtp: String,
    /// type "fixed": SIP address the PBX calls (no registration), e.g. sip:42@10.0.0.5:5070
    #[serde(default)]
    contact: String,
    /// type "mobile": the mobile number
    #[serde(default)]
    mobile: String,
}

/// Admin creates a device: Linphone gets a QR code, everything else gets credentials (shown once).
pub async fn admin_device_create(
    State(app): State<AppState>,
    Admin(u): Admin,
    Path(id): Path<i32>,
    Form(f): Form<AdminNewDevice>,
) -> Result<Response, AppError> {
    let label = clean_label(&f.label)?;
    let back = format!("/admin/users/{id}");
    if f.r#type == "mobile" {
        let e164 = crate::selfservice::mobile_number(&app, id, &f.mobile).await?;
        let device = db::create_mobile_device(&app.db, id, &label, &e164, "admin").await?;
        db::audit(&app.db, &u.session.sub, "device.add", &format!("device-id:{}", device.id), serde_json::json!({"type": "mobile", "managed_by": "admin"})).await?;
        return Ok(Redirect::to(&back).into_response());
    }
    // Linphone is provisioned by the PBX; anything else is a plain SIP device set up by hand
    // fixed address: checked before the device exists (same rule as the database CHECK)
    let contact = if f.r#type == "fixed" { Some(fixed_contact(&f.contact)?) } else { None };
    let (dtype, transport, srtp) = match f.r#type.as_str() {
        "linphone" => ("linphone", "tls", "required"),
        _ => {
            let (tr, s) = db::transport_srtp(&f.transport, &f.srtp);
            ("generic", tr, s)
        }
    };
    let (device, pass) = db::create_device(&app.db, &app.cfg.numbers, &app.cfg.sip_realm, id, &label, dtype, "admin", transport, srtp).await?;
    db::audit(&app.db, &u.session.sub, "device.add", &format!("device:{}", device.num()), serde_json::json!({"type": dtype, "managed_by": "admin"})).await?;
    if let Some(c) = contact {
        // no credentials to show: the device never registers, the PBX calls it
        sqlx::query("UPDATE devices SET contact_uri = $2 WHERE id = $1").bind(device.id).bind(&c).execute(&app.db).await.map_err(anyhow::Error::from)?;
        db::audit(&app.db, &u.session.sub, "device.contact", &format!("device:{}", device.num()), serde_json::json!({"contact": c})).await?;
        return Ok(Redirect::to(&back).into_response());
    }
    if dtype == "linphone" {
        return provision_page(&app, &u, device, &back).await;
    }
    render(CredentialsPage {
        c: Ctx::new(&app, &u).section("users"),
        device,
        realm: app.cfg.sip_realm.clone(),
        password: pass,
        back,
    })
}

pub async fn admin_device_action(
    State(app): State<AppState>,
    Admin(u): Admin,
    Path((uid, did)): Path<(i32, i32)>,
    Form(f): Form<DeviceAction>,
) -> Result<Response, AppError> {
    let d = db::device_of(&app.db, uid, did).await?.ok_or(AppError::NotFound)?;
    if f.action == "password_reset" || f.action == "password_set" {
        // plain SIP devices only: Linphone is provisioned (new QR code instead), mobiles have no account
        if d.r#type != "generic" {
            return Err(AppError::BadRequest("Only plain SIP devices have a password to set (Linphone: new QR code).".into()));
        }
        let pass = if f.action == "password_set" {
            let p = f.password.as_deref().unwrap_or("").trim().to_string();
            if !db::valid_sip_password(&p) {
                return Err(AppError::BadRequest("Password: 12 to 64 characters, no spaces or quotes.".into()));
            }
            p
        } else {
            db::random_string(24)
        };
        db::set_device_password(&app.db, &app.cfg.sip_realm, did, &d.sip_username, &pass).await?;
        db::audit(&app.db, &u.session.sub, "device.password", &format!("device:{}", d.num()), serde_json::json!({"manual": f.action == "password_set"})).await?;
        app.asterisk_kick(&d.sip_username).await;
        return render(CredentialsPage {
            c: Ctx::new(&app, &u).section("users"),
            device: d,
            realm: app.cfg.sip_realm.clone(),
            password: pass,
            back: format!("/admin/users/{uid}"),
        });
    }
    if f.action == "transport" {
        if d.is_mobile() {
            return Err(AppError::BadRequest("A mobile phone has no SIP transport.".into()));
        }
        let (tr, s) = db::transport_srtp(f.transport.as_deref().unwrap_or(""), f.srtp.as_deref().unwrap_or(""));
        let (tr, s) = if d.r#type == "linphone" { ("tls", "required") } else { (tr, s) };
        sqlx::query("UPDATE devices SET transport = $2::sip_transport, srtp = $3 WHERE id = $1")
            .bind(did)
            .bind(tr)
            .bind(s)
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        db::audit(&app.db, &u.session.sub, "device.transport", &format!("device:{}", d.num()), serde_json::json!({"transport": tr, "srtp": s})).await?;
        app.asterisk_kick(&d.sip_username).await;
        return Ok(Redirect::to(&format!("/admin/users/{uid}")).into_response());
    }
    let sql = match f.action.as_str() {
        "enable" => "UPDATE devices SET enabled = true, admin_locked = false WHERE id = $1",
        "disable" => "UPDATE devices SET enabled = false, admin_locked = true WHERE id = $1",
        "delete" => "UPDATE devices SET deleted_at = now(), enabled = false WHERE id = $1",
        _ => return Err(AppError::BadRequest("Unknown action.".into())),
    };
    sqlx::query(sql).bind(did).execute(&app.db).await.map_err(anyhow::Error::from)?;
    if f.action != "enable" {
        db::revoke_tokens(&app.db, did).await?;
        app.asterisk_kick(&d.sip_username).await;
    } else {
        app.asterisk_refresh(&d.sip_username).await;
    }
    db::audit(&app.db, &u.session.sub, &format!("device.{}", f.action), &format!("device:{}", d.num()), serde_json::json!({"admin": true})).await?;
    Ok(Redirect::to(&format!("/admin/users/{uid}")).into_response())
}

#[derive(Deserialize)]
pub struct PhonebookForm {
    #[serde(default)]
    listed: Option<String>,
}

/// The user decides whether they appear in the internal phone book.
pub async fn phonebook_visibility(State(app): State<AppState>, u: CurrentUser, Form(f): Form<PhonebookForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if app.cfg.phonebook_enforced {
        return Err(AppError::Forbidden("All users are listed in the phone book at this site.".into()));
    }
    sqlx::query("UPDATE users SET in_phonebook = $2 WHERE id = $1")
        .bind(uid)
        .bind(f.listed.is_some())
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "user.phonebook", &format!("user-id:{uid}"), serde_json::json!({"listed": f.listed.is_some()})).await?;
    Ok(Redirect::to("/").into_response())
}

#[derive(Deserialize)]
pub struct StatusQuery {
    user: Option<i32>,
}

/// Live registration state of a user's devices (own devices; admins may pass ?user=).
pub async fn devices_status(State(app): State<AppState>, u: CurrentUser, axum::extract::Query(q): axum::extract::Query<StatusQuery>) -> Result<Response, AppError> {
    let uid = match q.user {
        Some(id) if u.is_admin => id,
        Some(_) => return Err(AppError::Forbidden("Admins only.".into())),
        None => u.phone_uid()?,
    };
    let devices = db::with_registration(db::devices_of(&app.db, uid).await?).await;
    let map: serde_json::Map<String, serde_json::Value> = devices
        .iter()
        .map(|d| (d.id.to_string(), serde_json::json!({"enabled": d.enabled, "state": d.reg_state(), "detail": d.reg_detail()})))
        .collect();
    Ok(axum::Json(map).into_response())
}

#[derive(Deserialize)]
pub struct LinkForm {
    keycloak: String,
}

/// The OIDC account to link: Keycloak search (PBX_OIDC_SYNC=keycloak) or the subject (`sub`) as
/// entered by the admin (other providers; the user sees it on the start page after logging in).
async fn find_account(app: &AppState, q: &str) -> Result<crate::sync::FoundUser, AppError> {
    if !app.cfg.auth.oidc() {
        return Err(AppError::BadRequest("OIDC login is off (PBX_AUTH).".into()));
    }
    if app.cfg.kc_sync {
        return crate::sync::find_user(app, q).await.map_err(|e| AppError::BadRequest(format!("Keycloak: {e:#}")));
    }
    let sub = q.trim();
    if sub.is_empty()
        || sub.len() > 255
        || sub.chars().any(|c| c.is_whitespace() || c.is_control())
        || sub.starts_with(crate::auth::LOCAL_SUB)
        || sub.starts_with("automation:")
    {
        return Err(AppError::BadRequest("Invalid OIDC subject.".into()));
    }
    Ok(crate::sync::FoundUser { sub: sub.into(), username: sub.into(), email: None, name: String::new() })
}

/// Links an existing PBX user (e.g. migrated ones) to an OIDC account chosen by the admin.
pub async fn admin_user_link(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<i32>, Form(f): Form<LinkForm>) -> Result<Response, AppError> {
    let k = find_account(&app, &f.keycloak).await?;
    let n = sqlx::query("UPDATE users SET kc_subject = $2, email = COALESCE($3, email) WHERE id = $1 AND deleted_at IS NULL AND kind = 'person'")
        .bind(id)
        .bind(&k.sub)
        .bind(&k.email)
        .execute(&app.db)
        .await
        .map_err(|_| AppError::BadRequest(format!("Keycloak account {} is already linked to another user.", k.username)))?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    db::audit(&app.db, &u.session.sub, "user.link", &format!("user-id:{id}"), serde_json::json!({"keycloak": k.username, "sub": k.sub})).await?;
    Ok(Redirect::to(&format!("/admin/users/{id}")).into_response())
}

pub async fn admin_user_unlink(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let (sub, groups): (Option<String>, Vec<String>) = sqlx::query_as("SELECT kc_subject, kc_groups FROM users WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    crate::provision::refuse_if_idp_managed(&app, sub.as_deref(), &groups, "unlinked")?;
    sqlx::query("UPDATE users SET kc_subject = NULL WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "user.unlink", &format!("user-id:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/admin/users/{id}")).into_response())
}

#[cfg(test)]
mod presence_tests {
    use super::presence;

    #[test]
    fn maps_hint_states() {
        assert_eq!(presence(Some("Idle"), false), "available");
        assert_eq!(presence(Some("Idle"), true), "dnd");
        assert_eq!(presence(Some("InUse"), true), "busy");
        assert_eq!(presence(Some("InUse&Ringing"), false), "busy");
        assert_eq!(presence(Some("OnHold"), false), "busy");
        assert_eq!(presence(Some("Ringing"), false), "ringing");
        assert_eq!(presence(Some("Unavailable"), false), "offline");
        assert_eq!(presence(None, false), "offline");
    }
}
