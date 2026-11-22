// SPDX-License-Identifier: GPL-3.0-or-later
//! Client address behind a reverse proxy (PBX_TRUSTED_PROXIES).
//!
//! X-Forwarded-For is believed only when the TCP peer is a trusted proxy; the client is the right-most
//! address in the header that is not itself a trusted proxy (entries further left can be forged by
//! the client). Absolute URLs never come from forwarded headers: they are built from PBX_PUBLIC_URL.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use axum::http::HeaderMap;

/// One network in CIDR notation (a bare address is a /32 or /128).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    len: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (a, l) = match s.split_once('/') {
            Some((a, l)) => (a, Some(l)),
            None => (s, None),
        };
        let net: IpAddr = a.parse().with_context(|| format!("{s}: not an IP address or network"))?;
        let net = net.to_canonical();
        let max = if net.is_ipv4() { 32 } else { 128 };
        let len = match l {
            Some(l) => l.parse::<u8>().ok().filter(|l| *l <= max).with_context(|| format!("{s}: invalid prefix length"))?,
            None => max,
        };
        Ok(Cidr { net, len })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => prefix_eq(u32::from(n) as u128, u32::from(i) as u128, self.len, 32),
            (IpAddr::V6(n), IpAddr::V6(i)) => prefix_eq(u128::from(n), u128::from(i), self.len, 128),
            _ => false,
        }
    }
}

fn prefix_eq(a: u128, b: u128, len: u8, bits: u8) -> bool {
    if len == 0 {
        return true;
    }
    let shift = (bits - len) as u32;
    (a >> shift) == (b >> shift)
}

/// "10.0.0.0/8, 127.0.0.1" -> networks; empty string -> none.
pub fn parse_list(s: &str) -> Result<Vec<Cidr>> {
    s.split([',', ' ']).filter(|p| !p.trim().is_empty()).map(Cidr::parse).collect()
}

/// The client's address: the TCP peer, or with a trusted peer the right-most untrusted address in
/// X-Forwarded-For.
pub fn client_ip(trusted: &[Cidr], peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let peer_ip = peer.ip().to_canonical();
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer_ip) {
        return peer_ip;
    }
    let hops: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|h| h.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect();
    hops.iter().rev().copied().find(|ip| !is_trusted(*ip)).or_else(|| hops.first().copied()).unwrap_or(peer_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(xff: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("x-forwarded-for", xff.parse().unwrap());
        m
    }

    #[test]
    fn cidr() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(c.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(Cidr::parse("127.0.0.1").unwrap().contains("127.0.0.1".parse().unwrap()));
        assert!(!Cidr::parse("127.0.0.1").unwrap().contains("127.0.0.2".parse().unwrap()));
        assert!(Cidr::parse("fd00::/8").unwrap().contains("fd12::1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("example.com").is_err());
        assert_eq!(parse_list(" 10.0.0.0/8, 127.0.0.1 ").unwrap().len(), 2);
        assert!(parse_list("").unwrap().is_empty());
    }

    #[test]
    fn forwarded_for() {
        let peer: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let none: Vec<Cidr> = vec![];
        let lo = parse_list("127.0.0.1, 10.9.0.0/16").unwrap();
        // untrusted peer: header ignored
        assert_eq!(client_ip(&none, peer, &h("1.2.3.4")), "127.0.0.1".parse::<IpAddr>().unwrap());
        // trusted peer: right-most untrusted hop, forged left entries ignored
        assert_eq!(client_ip(&lo, peer, &h("6.6.6.6, 1.2.3.4")), "1.2.3.4".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(&lo, peer, &h("1.2.3.4, 10.9.0.5")), "1.2.3.4".parse::<IpAddr>().unwrap());
        // all hops trusted: left-most
        assert_eq!(client_ip(&lo, peer, &h("10.9.0.7, 10.9.0.5")), "10.9.0.7".parse::<IpAddr>().unwrap());
        // no header: the peer
        assert_eq!(client_ip(&lo, peer, &HeaderMap::new()), "127.0.0.1".parse::<IpAddr>().unwrap());
        // garbage: the peer
        assert_eq!(client_ip(&lo, peer, &h("unknown")), "127.0.0.1".parse::<IpAddr>().unwrap());
    }
}
