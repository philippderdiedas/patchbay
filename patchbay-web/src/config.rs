// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};

/// Configuration from environment variables; secrets are read from files.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    /// PBX_TLS=on (default): certificate and key (PBX_TLS_CERT/PBX_TLS_KEY); off: plain HTTP behind a
    /// reverse proxy that terminates TLS
    pub tls: Option<(String, String)>,
    /// PBX_TRUSTED_PROXIES: peers whose X-Forwarded-For is believed (client IP for login limits)
    pub trusted_proxies: Vec<crate::proxy::Cidr>,
    /// PBX_ASTERISK_CERT: certificate file Asterisk's TLS transport uses; a change restarts Asterisk
    /// when no call is active (None: off)
    pub asterisk_cert: Option<String>,
    /// PBX_ASTERISK_RESTART_MAX_WAIT: seconds to wait for calls to end before restarting anyway (0: never force)
    pub asterisk_restart_max_wait: u64,
    pub public_url: String,
    pub sip_realm: String,
    /// PBX_NUMBERS_*, PBX_NUMBER_COOLDOWN_DAYS, PBX_NUMBER_CHANGE_DAYS
    pub numbers: crate::numbers::NumberPlan,
    /// PBX_AUTH: which logins are offered
    pub auth: AuthMode,
    /// OIDC settings; Some exactly when `auth` includes OIDC
    pub oidc: Option<OidcConfig>,
    /// PBX_OIDC_SYNC=keycloak (and OIDC enabled): Keycloak sync job and Keycloak user search
    pub kc_sync: bool,
    /// PBX_PROVISION: who creates persons (admin, automatically for the phone group, or self-service)
    pub provision: Provision,
    pub cookie_key: Vec<u8>,
    pub group_admin: String,
    pub group_phone: String,
    pub group_webui: String,
    pub site_title: String,
    pub theme_css: Option<String>,
    pub smtp: Option<String>,
    /// true: every user is listed in the phone book (no opt-out)
    pub phonebook_enforced: bool,
    /// guard rails for custom dialplan apps (PBX_CUSTOM_DIALPLAN_GUARD=strict|off)
    pub custom_dialplan_guard: bool,
    pub apps_per_user: i64,
    /// PBX_DEVICES_PER_USER: devices a user may create themselves (admins: no limit)
    pub devices_per_user: i64,
    /// PBX_MOBILE_DEVICES=admin|users: who may add a mobile phone as a device (rings in parallel,
    /// dialled out over the trunks); default admin only
    pub mobile_devices_self: bool,
    /// PBX_TTS: runtime text to speech (Wyoming server), None = off
    pub tts: Option<crate::tts::TtsConfig>,
    /// PBX_SCRIPT_HTTP_ALLOW: hosts/networks script apps may fetch from (empty: public internet)
    pub script_http_allow: Vec<crate::script::Allow>,
    /// PBX_USER_APPS: users may create their own apps (company apps by admins are always possible);
    /// same as `!user_app_kinds.is_empty()`
    pub user_apps: bool,
    /// PBX_USER_APPS as a list (or on = the safe kinds): app kinds users may create and use
    pub user_app_kinds: Vec<String>,
    /// PBX_USER_MOH: users may choose their own hold music from their media (needs PBX_USER_APPS)
    pub user_moh: bool,
    /// PBX_USER_NOANSWER: users set their own ring time and no-answer target (mailbox, number, nothing)
    pub user_noanswer: bool,
    /// PBX_SHARED_NOTES (default off): call notes may be marked "visible to all colleagues"; off: no
    /// new shared notes, existing ones only visible to their author
    pub shared_notes: bool,
    pub media_quota_mb: i64,
    pub mail_from: Option<String>,
    /// PBX_INTERNAL_KEY_FILE: shared secret the dialplan sends to the internal webhooks (127.0.0.1:8081)
    pub internal_key: String,
    /// PBX_RETENTION_CALLS_DAYS: call records (cdr) older than this are deleted; 0 = keep forever
    pub retention_calls_days: i32,
    /// PBX_RETENTION_AUDIT_DAYS: audit log entries older than this are deleted; 0 = keep forever
    pub retention_audit_days: i32,
    /// PBX_TIMEZONE (else TZ, else UTC): time profiles and all times shown in the UI/bot
    pub timezone: String,
    /// PBX_HOLIDAY_NAGER_URL: Nager.Date API base for holiday imports (no trailing slash)
    pub holiday_nager_url: String,
    /// PBX_COUNTRY_CODE (required): own country calling code without +, for number normalisation
    /// (dial rules, E.164) and the Linphone dial prefix
    pub country_code: String,
    /// PBX_EMERGENCY_NUMBERS (default 112): short emergency numbers, 2–3 digits each
    pub emergency_numbers: Vec<String>,
    /// PBX_TRANSFER_RECALL_S (default 20, 0 = off): an unanswered blind transfer rings back to the
    /// transferring person after at most this many seconds (dialplan reads settings.transfer_recall_s)
    pub transfer_recall_s: u16,
    /// PBX_MONITOR_NETS (default this host): clients allowed to read /monitor and /metrics
    pub monitor_nets: Vec<crate::proxy::Cidr>,
}

/// PBX_PROVISION
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provision {
    /// admins create persons (default)
    Admin,
    /// every OIDC user in the phone group gets a person and number (login, Keycloak sync)
    Auto,
    /// OIDC users claim a number themselves on the start page (GURU style); phone permission
    /// does not need the phone group
    SelfService,
}

impl Provision {
    /// The phone group decides about phone permission (not in self-service).
    pub fn needs_phone_group(self) -> bool {
        self != Provision::SelfService
    }
}

/// PBX_AUTH=oidc|local|both
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthMode {
    Oidc,
    Local,
    Both,
}

impl AuthMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "" | "oidc" => Ok(AuthMode::Oidc),
            "local" => Ok(AuthMode::Local),
            "both" | "oidc,local" | "local,oidc" => Ok(AuthMode::Both),
            o => anyhow::bail!("PBX_AUTH={o}: expected oidc, local or both"),
        }
    }

    pub fn oidc(self) -> bool {
        matches!(self, AuthMode::Oidc | AuthMode::Both)
    }

    pub fn local(self) -> bool {
        matches!(self, AuthMode::Local | AuthMode::Both)
    }
}

/// Login settings the admin pages need.
#[derive(Clone, Copy)]
pub struct AuthUi {
    pub oidc: bool,
    pub local: bool,
    /// Keycloak search and sync (phone permission of linked users comes from the group)
    pub kc_sync: bool,
}

impl Config {
    pub fn auth_ui(&self) -> AuthUi {
        AuthUi { oidc: self.auth.oidc(), local: self.auth.local(), kc_sync: self.kc_sync }
    }
}

#[derive(Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
}

/// PBX_OIDC_SYNC=keycloak|off (only relevant with OIDC)
fn parse_sync(s: &str) -> Result<bool> {
    match s.trim().to_lowercase().as_str() {
        "" | "keycloak" => Ok(true),
        "off" | "none" => Ok(false),
        o => anyhow::bail!("PBX_OIDC_SYNC={o}: expected keycloak or off"),
    }
}

/// Database URL only (CLI subcommands that need nothing else).
pub fn database_url() -> Result<String> {
    var("PBX_DATABASE_URL").or_else(|_| file("PBX_DATABASE_URL_FILE"))
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("environment variable {name} missing"))
}

/// PBX_USER_APPS: on/true (all kinds in `apps::USER_SAFE_KINDS`), off/false (none) or a comma
/// separated list of kinds (allowlist: new, riskier kinds are never enabled by an update).
fn user_app_kinds(v: &str) -> anyhow::Result<Vec<String>> {
    let v = v.trim().to_lowercase();
    Ok(match v.as_str() {
        "1" | "true" | "yes" | "on" => crate::apps::USER_SAFE_KINDS.iter().map(|k| k.to_string()).collect(),
        "0" | "false" | "no" | "off" | "" => vec![],
        list => {
            let kinds: Vec<String> = list.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
            if let Some(bad) = kinds.iter().find(|k| !crate::apps::KINDS.contains(&k.as_str())) {
                anyhow::bail!("PBX_USER_APPS: unknown app kind {bad:?} (known: {})", crate::apps::KINDS.join(", "));
            }
            if let Some(bad) = kinds.iter().find(|k| crate::apps::COMPANY_ONLY_KINDS.contains(&k.as_str())) {
                anyhow::bail!("PBX_USER_APPS: {bad:?} is only available as an organization app");
            }
            kinds
        }
    })
}

/// Version of the running image (`git describe` at build time, ENV PBX_BUILD in the Dockerfile);
/// "dev" for local builds.
pub fn build_version() -> &'static str {
    static V: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| std::env::var("PBX_BUILD").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "dev".into()));
    &V
}

/// OIDC groups when PBX_GROUP_* are not set (neutral names; sites with other names set PBX_GROUP_*).
pub const DEFAULT_GROUP_ADMIN: &str = "pbx-admin";
pub const DEFAULT_GROUP_PHONE: &str = "pbx-phone";
pub const DEFAULT_GROUP_WEBUI: &str = "pbx-user";

/// Site stylesheet: PBX_THEME_CSS (a path; empty or "off": none), unset: /config/theme.css when
/// it exists (the small stack mounts $PBX_DIR/config there).
fn theme_css(var: Option<String>) -> Option<String> {
    let path = match var.as_deref().map(str::trim) {
        Some("" | "off") => return None,
        Some(p) => p.to_string(),
        None => "/config/theme.css".to_string(),
    };
    std::path::Path::new(&path).exists().then_some(path)
}

fn var_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// on/off switch (also true/false, yes/no, 1/0); empty or unset: `default`.
fn on_off(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name).unwrap_or_default().trim().to_lowercase().as_str() {
        "" => Ok(default),
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        o => anyhow::bail!("{name}={o}: expected on or off"),
    }
}

/// Number of days (0 = off); empty or unset: `default`.
fn days(name: &str, default: i32) -> Result<i32> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => {
            v.trim().parse::<i32>().ok().filter(|d| (0..=36500).contains(d)).with_context(|| format!("{name}: days 0–36500 (0 = keep forever)"))
        }
        _ => Ok(default),
    }
}

/// OIDC client secret from its file; an empty file fails at startup instead of at the first login.
fn oidc_secret(s: String) -> Result<String> {
    anyhow::ensure!(
        !s.is_empty(),
        "OIDC is enabled (PBX_AUTH) but the client secret is missing: put it into the file of \
         OIDC_CLIENT_SECRET_FILE ($PBX_DIR/secrets/patchbay-web/oidc_client_secret on the host) or set PBX_AUTH=local"
    );
    Ok(s)
}

fn file(name: &str) -> Result<String> {
    let path = var(name)?;
    Ok(std::fs::read_to_string(&path)
        .with_context(|| format!("{name}: {path} not readable"))?
        .trim()
        .to_string())
}

/// PBX_TIMEZONE, else TZ (POSIX ":Area/City" accepted), else UTC. Checked against the database at
/// startup.
fn timezone() -> String {
    [std::env::var("PBX_TIMEZONE").ok(), std::env::var("TZ").ok()]
        .into_iter()
        .flatten()
        .map(|s| s.trim().trim_start_matches(':').to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "UTC".into())
}

/// PBX_COUNTRY_CODE: 1–3 digits, optional leading + or 00.
fn country_code(s: &str) -> Result<String> {
    let c = s.trim().trim_start_matches('+').trim_start_matches("00");
    anyhow::ensure!(
        (1..=3).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_digit()) && !c.starts_with('0'),
        "PBX_COUNTRY_CODE={s}: expected a country calling code like 49"
    );
    Ok(c.to_string())
}

/// PBX_EMERGENCY_NUMBERS: comma-separated, 2–3 digits each (4-digit numbers are internal numbers).
fn emergency_numbers(s: &str) -> Result<Vec<String>> {
    let v: Vec<String> = s.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
    anyhow::ensure!(
        !v.is_empty() && v.iter().all(|n| (2..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())),
        "PBX_EMERGENCY_NUMBERS={s}: expected a list of 2–3 digit numbers like 110,112"
    );
    Ok(v)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cookie_key = hex::decode(file("PBX_COOKIE_KEY_FILE")?).context("cookie key is not hex")?;
        anyhow::ensure!(cookie_key.len() >= 64, "cookie key must be at least 64 bytes");
        let hostname = var("PBX_HOSTNAME")?;
        let auth = AuthMode::parse(&var_or("PBX_AUTH", "oidc"))?;
        let oidc = if auth.oidc() {
            Some(OidcConfig {
                issuer: var("OIDC_ISSUER")?,
                client_id: var("OIDC_CLIENT_ID")?,
                client_secret: oidc_secret(file("OIDC_CLIENT_SECRET_FILE")?)?,
                scopes: var_or("OIDC_SCOPES", "openid email profile membership")
                    .split_whitespace()
                    .filter(|s| *s != "openid")
                    .map(String::from)
                    .collect(),
            })
        } else {
            None
        };
        let kc_sync = oidc.is_some() && parse_sync(&var_or("PBX_OIDC_SYNC", "keycloak"))?;
        let provision = match var_or("PBX_PROVISION", "admin").trim() {
            "admin" => Provision::Admin,
            "auto" => Provision::Auto,
            "self" => Provision::SelfService,
            v => anyhow::bail!("PBX_PROVISION must be admin, auto or self (got {v:?})"),
        };
        anyhow::ensure!(provision == Provision::Admin || oidc.is_some(), "PBX_PROVISION=auto/self needs an OIDC login (PBX_AUTH)");
        let tls = match on_off("PBX_TLS", true)? {
            true => Some((var_or("PBX_TLS_CERT", "/keys/voip.crt"), var_or("PBX_TLS_KEY", "/keys/voip.key"))),
            false => None,
        };
        let default_listen = if tls.is_some() { "0.0.0.0:443" } else { "0.0.0.0:8080" };
        let user_kinds = user_app_kinds(&var_or("PBX_USER_APPS", "true"))?;
        Ok(Self {
            database_url: database_url()?,
            listen: std::env::var("PBX_LISTEN").ok().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| default_listen.into()),
            tls,
            provision,
            trusted_proxies: crate::proxy::parse_list(&var_or("PBX_TRUSTED_PROXIES", "")).context("PBX_TRUSTED_PROXIES")?,
            asterisk_cert: match var_or("PBX_ASTERISK_CERT", "/keys/voip.crt").trim() {
                "" | "off" => None,
                p => Some(p.to_string()),
            },
            asterisk_restart_max_wait: var_or("PBX_ASTERISK_RESTART_MAX_WAIT", "3600")
                .trim()
                .parse()
                .context("PBX_ASTERISK_RESTART_MAX_WAIT: seconds (0 = wait for the calls to end, however long)")?,
            public_url: var_or("PBX_PUBLIC_URL", &format!("https://{hostname}")),
            sip_realm: var_or("PBX_SIP_REALM", &hostname),
            numbers: crate::numbers::NumberPlan::from_env()?,
            auth,
            oidc,
            kc_sync,
            cookie_key,
            group_admin: var_or("PBX_GROUP_ADMIN", DEFAULT_GROUP_ADMIN),
            group_phone: var_or("PBX_GROUP_PHONE", DEFAULT_GROUP_PHONE),
            group_webui: var_or("PBX_GROUP_WEBUI", DEFAULT_GROUP_WEBUI),
            site_title: var_or("PBX_SITE_TITLE", "Phone system"),
            theme_css: theme_css(std::env::var("PBX_THEME_CSS").ok()),
            smtp: std::env::var("PBX_SMTP").ok().filter(|s| !s.is_empty()),
            phonebook_enforced: var_or("PBX_PHONEBOOK", "optional") == "enforced",
            custom_dialplan_guard: var_or("PBX_CUSTOM_DIALPLAN_GUARD", "strict") != "off",
            apps_per_user: var_or("PBX_APPS_PER_USER", "10").parse().unwrap_or(10),
            devices_per_user: var_or("PBX_DEVICES_PER_USER", "10").parse().unwrap_or(10),
            mobile_devices_self: var_or("PBX_MOBILE_DEVICES", "admin") == "users",
            tts: crate::tts::TtsConfig::from_env()?,
            script_http_allow: crate::script::parse_allow(&var_or("PBX_SCRIPT_HTTP_ALLOW", ""))?,
            user_apps: !user_kinds.is_empty(),
            user_moh: matches!(var_or("PBX_USER_MOH", "false").to_lowercase().as_str(), "1" | "true" | "yes" | "on")
                && !user_kinds.is_empty(),
            user_app_kinds: user_kinds,
            user_noanswer: matches!(var_or("PBX_USER_NOANSWER", "true").to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            shared_notes: on_off("PBX_SHARED_NOTES", false)?,
            media_quota_mb: var_or("PBX_MEDIA_MAX_MB", "200").parse().unwrap_or(200),
            mail_from: std::env::var("PBX_MAIL_FROM").ok().filter(|s| !s.is_empty()),
            internal_key: {
                let k = file("PBX_INTERNAL_KEY_FILE")?;
                anyhow::ensure!(k.len() >= 32, "PBX_INTERNAL_KEY_FILE: key too short (patchbay-init creates it)");
                k
            },
            retention_calls_days: days("PBX_RETENTION_CALLS_DAYS", 0)?,
            retention_audit_days: days("PBX_RETENTION_AUDIT_DAYS", 0)?,
            timezone: timezone(),
            holiday_nager_url: var_or("PBX_HOLIDAY_NAGER_URL", "https://date.nager.at").trim().trim_end_matches('/').to_string(),
            country_code: country_code(&var("PBX_COUNTRY_CODE").context("PBX_COUNTRY_CODE is required, e.g. 49")?)?,
            emergency_numbers: emergency_numbers(&var_or("PBX_EMERGENCY_NUMBERS", "112"))?,
            transfer_recall_s: var_or("PBX_TRANSFER_RECALL_S", "20")
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|s| *s <= 300)
                .context("PBX_TRANSFER_RECALL_S: seconds 0–300 (0 = off)")?,
            monitor_nets: crate::proxy::parse_list(&var_or("PBX_MONITOR_NETS", "127.0.0.1/32,::1/128")).context("PBX_MONITOR_NETS")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_modes() {
        assert_eq!(AuthMode::parse("").unwrap(), AuthMode::Oidc);
        assert_eq!(AuthMode::parse("OIDC").unwrap(), AuthMode::Oidc);
        assert_eq!(AuthMode::parse("local").unwrap(), AuthMode::Local);
        assert_eq!(AuthMode::parse("both").unwrap(), AuthMode::Both);
        assert_eq!(AuthMode::parse("oidc,local").unwrap(), AuthMode::Both);
        assert!(AuthMode::parse("ldap").is_err());
        assert!(AuthMode::Both.oidc() && AuthMode::Both.local());
        assert!(!AuthMode::Local.oidc() && !AuthMode::Oidc.local());
        assert!(parse_sync("keycloak").unwrap());
        assert!(!parse_sync("off").unwrap());
        assert!(parse_sync("yes").is_err());
    }

    #[test]
    fn oidc_secret_empty() {
        assert!(oidc_secret(String::new()).is_err());
        assert_eq!(oidc_secret("s3cr3t".into()).unwrap(), "s3cr3t");
    }

    #[test]
    fn country_codes() {
        assert_eq!(country_code("49").unwrap(), "49");
        assert_eq!(country_code("+43").unwrap(), "43");
        assert_eq!(country_code("0041").unwrap(), "41");
        assert!(country_code("").is_err());
        assert!(country_code("1234").is_err());
        assert!(country_code("4x").is_err());
    }

    #[test]
    fn emergency_number_lists() {
        assert_eq!(emergency_numbers("110, 112").unwrap(), vec!["110", "112"]);
        assert_eq!(emergency_numbers("911").unwrap(), vec!["911"]);
        assert!(emergency_numbers("").is_err());
        assert!(emergency_numbers("1120").is_err());
        assert!(emergency_numbers("11a").is_err());
    }
}

#[cfg(test)]
mod user_apps_tests {
    use super::user_app_kinds;

    #[test]
    fn allowlist() {
        assert_eq!(user_app_kinds("on").unwrap(), crate::apps::USER_SAFE_KINDS);
        assert!(user_app_kinds("false").unwrap().is_empty());
        assert_eq!(user_app_kinds("echo, clock").unwrap(), ["echo", "clock"]);
        assert!(user_app_kinds("echo,rocket").is_err(), "unknown kinds are refused at startup");
        assert!(user_app_kinds("echo,gateway").is_err(), "organization-only kinds are refused");
    }

    #[test]
    fn theme() {
        let f = std::env::temp_dir().join("pbx-theme-test.css");
        std::fs::write(&f, "").unwrap();
        let p = f.to_str().unwrap().to_string();
        assert_eq!(super::theme_css(Some(p.clone())), Some(p));
        assert_eq!(super::theme_css(Some("off".into())), None);
        assert_eq!(super::theme_css(Some(" ".into())), None);
        assert_eq!(super::theme_css(Some("/nonexistent/theme.css".into())), None);
    }
}
