// SPDX-License-Identifier: GPL-3.0-or-later
//! Script apps (app kind "script"): a small Rhai script runs when the app is called and produces
//! text that is spoken (tts.rs) – "fetch something and read it out".
//!
//! Sandbox: Rhai has no file or network access of its own; modules (`import`) and `eval` are
//! disabled; operations, string/array/map sizes, call depth and run time are limited. The only
//! way out is `http_get`: GET only, at most MAX_REQUESTS per run, timeout, size limit, no
//! redirects, and the target address is checked after DNS resolution and then connected to
//! directly (no DNS rebinding): internal networks (loopback, private, link-local, CGNAT, …) only
//! when listed in PBX_SCRIPT_HTTP_ALLOW; with a non-empty allowlist only listed hosts/networks.
//!
//! Script API: `say(text)`, `say(text, lang)`, `http_get(url)`, `json(text)`, `caller()`,
//! `lang()`, `print(x)` (log, shown in the test run).

use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rhai::{Dynamic, Engine, EvalAltResult};

pub const MAX_SOURCE: usize = 20_000;
const MAX_REQUESTS: usize = 3;
const MAX_BODY: usize = 256 * 1024;
const MAX_SAYS: usize = 20;
const MAX_SAY_CHARS: usize = 3000;
const RUN_TIME: Duration = Duration::from_secs(10);

/// One entry of PBX_SCRIPT_HTTP_ALLOW: a domain (also its subdomains) or a network in CIDR form.
#[derive(Clone, Debug, PartialEq)]
pub enum Allow {
    Domain(String),
    Net(IpAddr, u8),
}

pub fn parse_allow(v: &str) -> anyhow::Result<Vec<Allow>> {
    let mut out = vec![];
    for e in v.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (addr, bits) = e.split_once('/').map(|(a, b)| (a, Some(b))).unwrap_or((e, None));
        if let Ok(ip) = addr.parse::<IpAddr>() {
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits = match bits {
                Some(b) => b.parse::<u8>().ok().filter(|b| *b <= max).ok_or_else(|| anyhow::anyhow!("PBX_SCRIPT_HTTP_ALLOW: bad prefix in {e:?}"))?,
                None => max,
            };
            out.push(Allow::Net(ip, bits));
        } else {
            let d = e.trim_start_matches("*.").trim_end_matches('.').to_lowercase();
            anyhow::ensure!(
                bits.is_none() && !d.is_empty() && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
                "PBX_SCRIPT_HTTP_ALLOW: {e:?} is neither a domain nor a network"
            );
            out.push(Allow::Domain(d));
        }
    }
    Ok(out)
}

fn in_net(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            (u32::from(a) & mask) == (u32::from(n) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            (u128::from(a) & mask) == (u128::from(n) & mask)
        }
        _ => false,
    }
}

/// Addresses scripts never reach unless an admin lists them explicitly.
fn internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast() || v.is_multicast() || v.is_documentation()
                || in_net(ip, "100.64.0.0".parse().unwrap(), 10)
                || in_net(ip, "198.18.0.0".parse().unwrap(), 15)
                || v.octets()[0] >= 240
        }
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return internal(IpAddr::V4(m));
            }
            v.is_loopback() || v.is_unspecified() || v.is_multicast()
                || in_net(ip, "fc00::".parse().unwrap(), 7)
                || in_net(ip, "fe80::".parse().unwrap(), 10)
                || in_net(ip, "2001:db8::".parse().unwrap(), 32)
        }
    }
}

/// May a script connect to `ip` reached via `host`?
pub fn allowed(host: &str, ip: IpAddr, allow: &[Allow]) -> bool {
    let host = host.to_lowercase();
    let by_host = allow.iter().any(|a| matches!(a, Allow::Domain(d) if host == *d || host.ends_with(&format!(".{d}"))));
    let by_net = allow.iter().any(|a| matches!(a, Allow::Net(n, b) if in_net(ip, *n, *b)));
    if internal(ip) {
        return by_host || by_net;
    }
    allow.is_empty() || by_host || by_net
}

async fn fetch(url: &str, allow: &[Allow]) -> Result<String, String> {
    let u = reqwest::Url::parse(url).map_err(|_| format!("bad URL: {url}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err("only http and https".into());
    }
    let host = u.host_str().ok_or("URL without host")?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![(ip, port).into()],
        Err(_) => tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| "DNS timeout".to_string())?
            .map_err(|_| format!("cannot resolve {host}"))?
            .collect(),
    };
    // every address must be allowed (a name with a public and an internal address is refused)
    let Some(addr) = addrs.first().copied() else { return Err(format!("cannot resolve {host}")) };
    if let Some(bad) = addrs.iter().find(|a| !allowed(&host, a.ip(), allow)) {
        return Err(format!("{host} ({}) is not allowed for scripts", bad.ip()));
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .resolve(&host, addr)
        .user_agent("pbx-script/1")
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(u).send().await.map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY {
            return Err("response too large".into());
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[derive(Debug, Default)]
pub struct Output {
    /// (language, text) to speak, in order
    pub says: Vec<(String, String)>,
    /// print() lines and the error, for the test run
    pub log: Vec<String>,
}

fn err(msg: impl Into<String>) -> Box<EvalAltResult> {
    msg.into().into()
}

/// Runs a script. `Err` = compile or runtime error (message for the owner).
pub async fn run(source: &str, caller: &str, lang: &str, allow: Vec<Allow>) -> Result<Output, (String, Output)> {
    let source = source.to_string();
    let (caller, lang) = (caller.to_string(), if lang == "de" { "de".to_string() } else { "en".to_string() });
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let out = Arc::new(Mutex::new(Output::default()));
        let requests = Arc::new(Mutex::new(0usize));
        let mut engine = Engine::new();
        engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
        engine.disable_symbol("eval");
        engine.set_max_operations(500_000);
        engine.set_max_string_size(MAX_BODY + 1024);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(10_000);
        engine.set_max_call_levels(32);
        engine.set_max_expr_depths(64, 32);
        let deadline = Instant::now() + RUN_TIME;
        engine.on_progress(move |_| if Instant::now() > deadline { Some(Dynamic::from("time limit")) } else { None });
        let o = out.clone();
        engine.on_print(move |s| {
            let mut o = o.lock().unwrap();
            if o.log.len() < 50 {
                o.log.push(s.chars().take(500).collect());
            }
        });
        engine.on_debug(|_, _, _| {});
        let say = {
            let o = out.clone();
            move |text: &str, l: &str| -> Result<(), Box<EvalAltResult>> {
                let mut o = o.lock().unwrap();
                let total: usize = o.says.iter().map(|(_, t)| t.chars().count()).sum();
                if o.says.len() >= MAX_SAYS || total + text.chars().count() > MAX_SAY_CHARS {
                    return Err(err("too much text"));
                }
                if !text.trim().is_empty() {
                    o.says.push((if l == "de" { "de".into() } else { "en".into() }, text.trim().to_string()));
                }
                Ok(())
            }
        };
        let say2 = say.clone();
        let l1 = lang.clone();
        engine.register_fn("say", move |text: &str| say(text, &l1));
        engine.register_fn("say", move |text: &str, l: &str| say2(text, l));
        let (c1, l2) = (caller.clone(), lang.clone());
        engine.register_fn("caller", move || c1.clone());
        engine.register_fn("lang", move || l2.clone());
        engine.register_fn("json", |s: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            let v: serde_json::Value = serde_json::from_str(s).map_err(|e| err(format!("json: {e}")))?;
            rhai::serde::to_dynamic(v)
        });
        engine.register_fn("http_get", move |url: &str| -> Result<String, Box<EvalAltResult>> {
            {
                let mut n = requests.lock().unwrap();
                if *n >= MAX_REQUESTS {
                    return Err(err(format!("at most {MAX_REQUESTS} requests per call")));
                }
                *n += 1;
            }
            handle.block_on(fetch(url, &allow)).map_err(|e| err(format!("http_get: {e}")))
        });
        let res = engine.run(&source);
        let mut o = std::mem::take(&mut *out.lock().unwrap());
        match res {
            Ok(()) => Ok(o),
            Err(e) => {
                let msg = e.to_string();
                o.log.push(format!("error: {msg}"));
                Err((msg, o))
            }
        }
    })
    .await
    .unwrap_or_else(|e| Err((format!("script crashed: {e}"), Output::default())))
}

/// Syntax check when saving.
pub fn check(source: &str) -> Result<(), String> {
    let mut engine = Engine::new();
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.compile(source).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_and_internal_networks() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // empty allowlist: public yes, internal no
        assert!(allowed("api.example.org", ip("93.184.216.34"), &[]));
        for i in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.169.254", "100.64.1.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "0.0.0.0"] {
            assert!(!allowed("x", ip(i), &[]), "{i} must be blocked");
        }
        let allow = parse_allow("example.org, 10.20.11.0/24").unwrap();
        assert!(allowed("spaceapi.example.org", ip("93.184.216.34"), &allow));
        assert!(!allowed("other.net", ip("93.184.216.34"), &allow), "non-empty allowlist: only listed");
        assert!(allowed("lab", ip("10.20.11.5"), &allow), "internal network listed explicitly");
        assert!(!allowed("lab", ip("10.20.12.5"), &allow));
        assert!(parse_allow("10.0.0.0/33").is_err());
        assert!(parse_allow("bad host!").is_err());
    }

    #[tokio::test]
    async fn sandbox() {
        let o = run(r#"let d = json("{\"t\": 21.5}"); say("Es sind " + d.t + " Grad"); print("ok");"#, "1020", "de", vec![]).await.unwrap();
        assert_eq!(o.says, [("de".to_string(), "Es sind 21.5 Grad".to_string())]);
        assert_eq!(o.log, ["ok"]);
        // no modules, no endless loops, no internal requests
        assert!(run(r#"import "x" as y;"#, "", "en", vec![]).await.is_err());
        assert!(run("loop { }", "", "en", vec![]).await.is_err());
        let (msg, _) = run(r#"http_get("http://127.0.0.1:8081/")"#, "", "en", vec![]).await.unwrap_err();
        assert!(msg.contains("not allowed"), "{msg}");
    }
}
