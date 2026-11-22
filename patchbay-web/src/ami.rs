// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal Asterisk Manager client (local only, 127.0.0.1:5038).

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

pub type Message = HashMap<String, String>;

struct Conn {
    rd: BufReader<tokio::net::tcp::OwnedReadHalf>,
    wr: tokio::net::tcp::OwnedWriteHalf,
}

impl Conn {
    /// Short-lived command connection: no events (they would only be skipped while waiting for
    /// the response).
    async fn open() -> Result<Self> {
        Self::open_with(Some("off")).await
    }

    /// `events`: event mask for this connection (e.g. "call,user", "off"); None = Asterisk's default
    async fn open_with(events: Option<&str>) -> Result<Self> {
        let secret = std::env::var("PBX_AMI_SECRET_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .context("PBX_AMI_SECRET_FILE missing")?;
        let s = timeout(Duration::from_secs(3), TcpStream::connect("127.0.0.1:5038")).await??;
        let (r, w) = s.into_split();
        let mut c = Conn { rd: BufReader::new(r), wr: w };
        let mut banner = String::new();
        c.rd.read_line(&mut banner).await?;
        let mut login = vec![("Action", "Login"), ("Username", "pbxweb"), ("Secret", secret.trim())];
        if let Some(e) = events {
            login.push(("Events", e));
        }
        c.send(&login).await?;
        let resp = loop {
            let m = c.read_msg().await?;
            if m.contains_key("Response") {
                break m;
            }
        };
        if resp.get("Response").map(String::as_str) != Some("Success") {
            bail!("AMI login failed");
        }
        Ok(c)
    }

    async fn send(&mut self, fields: &[(&str, &str)]) -> Result<()> {
        let msg = encode(fields)?;
        self.wr.write_all(msg.as_bytes()).await?;
        Ok(())
    }

    async fn read_msg(&mut self) -> Result<Message> {
        self.read_msg_t(Duration::from_secs(5)).await?.context("AMI timeout")
    }

    /// Next message; None when nothing arrived within `idle` (connection still usable).
    async fn read_msg_t(&mut self, idle: Duration) -> Result<Option<Message>> {
        let mut m = Message::new();
        loop {
            let mut line = String::new();
            let n = match timeout(idle, self.rd.read_line(&mut line)).await {
                Ok(r) => r?,
                Err(_) if m.is_empty() && line.is_empty() => return Ok(None),
                Err(_) => bail!("AMI timeout within a message"),
            };
            if n == 0 {
                bail!("AMI connection closed");
            }
            let line = line.trim_end();
            if line.is_empty() {
                if m.is_empty() {
                    continue;
                }
                return Ok(Some(m));
            }
            if let Some((k, v)) = line.split_once(": ") {
                // collect "Output" lines from Command
                m.entry(k.to_string())
                    .and_modify(|e: &mut String| {
                        e.push('\n');
                        e.push_str(v);
                    })
                    .or_insert_with(|| v.to_string());
            }
        }
    }

    async fn close(mut self) {
        let _ = self.send(&[("Action", "Logoff")]).await;
    }
}

/// One AMI message. A line break in a key or value would end the header and inject further
/// headers or actions: rejected.
fn encode(fields: &[(&str, &str)]) -> Result<String> {
    let mut msg = String::new();
    for (k, v) in fields {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            bail!("AMI {}: line break in value rejected", k.escape_debug());
        }
        msg += &format!("{k}: {v}\r\n");
    }
    msg += "\r\n";
    Ok(msg)
}

/// Long-lived connection that receives events (call tracking). Sends a Ping when idle and
/// fails when Asterisk stops answering, so the caller can reconnect.
pub struct EventConn {
    c: Conn,
    pings: u8,
}

impl EventConn {
    pub async fn open(events: &str) -> Result<Self> {
        Ok(EventConn { c: Conn::open_with(Some(events)).await?, pings: 0 })
    }

    pub async fn next(&mut self) -> Result<Message> {
        loop {
            match self.c.read_msg_t(Duration::from_secs(30)).await? {
                Some(m) => {
                    self.pings = 0;
                    if m.contains_key("Event") {
                        return Ok(m);
                    }
                }
                None => {
                    self.pings += 1;
                    if self.pings > 2 {
                        bail!("AMI event connection idle, no answer to Ping");
                    }
                    self.c.send(&[("Action", "Ping")]).await?;
                }
            }
        }
    }
}

/// Runs a list action (e.g. PJSIPShowContacts) and collects all events up to "…Complete".
pub async fn list(action: &str, event: &str) -> Result<Vec<Message>> {
    let mut c = Conn::open().await?;
    c.send(&[("Action", action), ("ActionID", "pbxweb")]).await?;
    let mut out = vec![];
    loop {
        let m = c.read_msg().await?;
        if m.get("Response").map(String::as_str) == Some("Error") {
            // e.g. "No Contacts found"
            break;
        }
        match m.get("Event").map(String::as_str) {
            Some(e) if e == event => out.push(m),
            Some(e) if e.ends_with("Complete") => break,
            _ => {}
        }
    }
    c.close().await;
    Ok(out)
}

/// Drops a device from Asterisk's realtime caches (sorcery.conf memory caches pbx_endpoint/pbx_auth/pbx_aor),
/// so changes (lock, delete, caller ID, transport) apply immediately instead of after the stale time.
pub async fn expire_device(sip_username: &str) -> Result<()> {
    let mut c = Conn::open().await?;
    for cache in ["pbx_endpoint", "pbx_auth", "pbx_aor"] {
        c.send(&[("Action", "SorceryMemoryCacheExpireObject"), ("Cache", cache), ("Object", sip_username)]).await?;
        // "Error" just means the object was not cached
        let _ = c.read_msg().await;
    }
    c.close().await;
    Ok(())
}

/// Empties the device caches (sorcery.conf memory caches) completely: devices are re-read from the
/// database on next use. patchbay-web does this at startup, so changes made while it was down (migrations,
/// restores) apply even when Asterisk kept running.
pub async fn expire_device_caches() -> Result<()> {
    let mut c = Conn::open().await?;
    for cache in ["pbx_endpoint", "pbx_auth", "pbx_aor"] {
        c.send(&[("Action", "SorceryMemoryCacheExpire"), ("Cache", cache)]).await?;
        let m = c.read_msg().await?;
        if m.get("Response").map(String::as_str) != Some("Success") {
            bail!("expiring cache {cache}: {}", m.get("Message").cloned().unwrap_or_default());
        }
    }
    c.close().await;
    Ok(())
}

/// Seconds since Asterisk (re)started ("core show uptime seconds").
pub async fn uptime_seconds() -> Result<u64> {
    parse_uptime(&command("core show uptime seconds").await?).context("unexpected output of core show uptime")
}

fn parse_uptime(out: &str) -> Option<u64> {
    out.lines().find_map(|l| l.trim().strip_prefix("System uptime:")).and_then(|v| v.trim().parse().ok())
}

/// Hangs up all channels of an endpoint.
pub async fn hangup_endpoint(endpoint: &str) -> Result<()> {
    let mut c = Conn::open().await?;
    let chan = format!("/^PJSIP\\/{endpoint}-/");
    c.send(&[("Action", "Hangup"), ("Channel", &chan)]).await?;
    let _ = c.read_msg().await;
    c.close().await;
    Ok(())
}

/// One action, returns its response (skips unsolicited events).
pub async fn action(fields: &[(&str, &str)]) -> Result<Message> {
    let mut c = Conn::open().await?;
    c.send(fields).await?;
    let resp = loop {
        let m = c.read_msg().await?;
        if m.contains_key("Response") {
            break m;
        }
    };
    c.close().await;
    Ok(resp)
}

/// Originate into an application (e.g. PickupChan) instead of a dialplan location; true on success.
pub async fn originate_app(channel: &str, app: &str, data: &str, callerid: &str) -> Result<bool> {
    let r = action(&[
        ("Action", "Originate"),
        ("Channel", channel),
        ("Application", app),
        ("Data", data),
        ("CallerID", callerid),
        ("Timeout", "30000"),
        ("Async", "true"),
    ])
    .await?;
    Ok(r.get("Response").map(String::as_str) == Some("Success"))
}

/// Starts a call asynchronously (Action: Originate). Variables are "NAME=value" pairs.
pub async fn originate(channel: &str, context: &str, exten: &str, callerid: &str, vars: &[(&str, &str)]) -> Result<()> {
    let mut c = Conn::open().await?;
    let vars: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut fields: Vec<(&str, &str)> = vec![
        ("Action", "Originate"),
        ("Channel", channel),
        ("Context", context),
        ("Exten", exten),
        ("Priority", "1"),
        ("CallerID", callerid),
        ("Timeout", "30000"),
        ("Async", "true"),
    ];
    for v in &vars {
        fields.push(("Variable", v));
    }
    c.send(&fields).await?;
    // Skip unsolicited events (e.g. FullyBooted) until the action response arrives
    let resp = loop {
        let m = c.read_msg().await?;
        if m.contains_key("Response") {
            break m;
        }
    };
    c.close().await;
    if resp.get("Response").map(String::as_str) != Some("Success") {
        bail!("originate failed: {}", resp.get("Message").cloned().unwrap_or_default());
    }
    Ok(())
}

/// Whether an endpoint currently has a registered contact.
/// Registration of a device as Asterisk sees it (one contact per device, max_contacts = 1).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Contact {
    /// "Reachable", "Unreachable", "Unknown" …
    pub status: String,
    pub rtt_ms: Option<f64>,
    pub user_agent: String,
    /// IP:port the device registered from
    pub addr: String,
}

/// All registered contacts, keyed by endpoint (= device SIP username).
pub async fn contacts() -> Result<std::collections::HashMap<String, Contact>> {
    let mut m = std::collections::HashMap::new();
    for e in list("PJSIPShowContacts", "ContactList").await? {
        let Some(ep) = e.get("Endpoint").filter(|s| !s.is_empty()) else { continue };
        let uri = e.get("Uri").cloned().unwrap_or_default();
        let addr = uri.split('@').nth(1).unwrap_or(&uri).split(';').next().unwrap_or("").to_string();
        let rtt_ms = e.get("RoundtripUsec").and_then(|r| r.parse::<f64>().ok()).filter(|r| *r > 0.0).map(|r| (r / 100.0).round() / 10.0);
        m.insert(ep.clone(), Contact {
            status: e.get("Status").cloned().unwrap_or_default(),
            rtt_ms,
            user_agent: e.get("UserAgent").cloned().unwrap_or_default(),
            addr,
        });
    }
    Ok(m)
}

pub async fn is_registered(endpoint: &str) -> Result<bool> {
    Ok(list("PJSIPShowContacts", "ContactList")
        .await?
        .iter()
        .any(|m| m.get("Endpoint").map(String::as_str) == Some(endpoint)))
}

/// Names of all PJSIP endpoints Asterisk has loaded (config and realtime).
pub async fn pjsip_endpoints() -> Result<std::collections::HashSet<String>> {
    Ok(list("PJSIPShowEndpoints", "EndpointList").await?.into_iter().filter_map(|mut m| m.remove("ObjectName")).collect())
}

/// Outbound registrations: name -> Status ("Registered", "Unregistered", "Rejected", ...).
pub async fn outbound_registrations() -> Result<std::collections::HashMap<String, String>> {
    Ok(list("PJSIPShowRegistrationsOutbound", "OutboundRegistrationDetail")
        .await?
        .into_iter()
        .filter_map(|mut m| Some((m.remove("ObjectName")?, m.remove("Status").unwrap_or_default())))
        .collect())
}

/// IAX2 peers: name -> Status ("OK (12 ms)", "UNREACHABLE", "Unmonitored", ...).
pub async fn iax_peers() -> Result<std::collections::HashMap<String, String>> {
    Ok(list("IAXpeerlist", "PeerEntry")
        .await?
        .into_iter()
        .filter_map(|mut m| Some((m.remove("ObjectName")?, m.remove("Status").unwrap_or_default())))
        .collect())
}

/// Runs a CLI command and returns its output.
pub async fn command(cmd: &str) -> Result<String> {
    let mut c = Conn::open().await?;
    c.send(&[("Action", "Command"), ("Command", cmd)]).await?;
    let resp = loop {
        let m = c.read_msg().await?;
        if m.contains_key("Response") {
            break m;
        }
    };
    c.close().await;
    Ok(resp.get("Output").cloned().unwrap_or_default())
}

/// Hint states of the internal extensions (context "hints"): extension -> Asterisk StatusText
/// (Idle, InUse, Busy, Ringing, OnHold, InUse&Ringing, Unavailable, ...).
pub async fn extension_states() -> Result<std::collections::HashMap<String, String>> {
    let mut m = std::collections::HashMap::new();
    for e in list("ExtensionStateList", "ExtensionStatus").await? {
        if e.get("Context").map(String::as_str) != Some("hints") {
            continue;
        }
        if let (Some(ext), Some(st)) = (e.get("Exten"), e.get("StatusText")) {
            m.insert(ext.clone(), st.clone());
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    #[test]
    fn uptime_output() {
        assert_eq!(super::parse_uptime("System uptime: 8\nLast reload: 8"), Some(8));
        assert_eq!(super::parse_uptime("Privilege: Command\nSystem uptime: 12345"), Some(12345));
        assert_eq!(super::parse_uptime("No such command"), None);
    }

    #[test]
    fn encode_rejects_line_breaks() {
        assert_eq!(super::encode(&[("Action", "Ping")]).unwrap(), "Action: Ping\r\n\r\n");
        assert!(super::encode(&[("Action", "Command"), ("Command", "core show version\r\nAction: Originate")]).is_err());
        assert!(super::encode(&[("Object", "a\nb")]).is_err());
        assert!(super::encode(&[("X\r", "v")]).is_err());
    }
}
