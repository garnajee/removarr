//! Trusted-proxy handling. `X-Forwarded-For` is honoured only when the TCP peer is inside an
//! explicitly configured CIDR; entries are consumed right to left so a client cannot spoof by
//! prepending addresses.

use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
}

impl TrustedProxies {
    /// No trusted proxy: the peer address is always the client.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Parses CIDRs or bare IPs. A zero-length prefix (trust everyone) is rejected.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, &'static str> {
        let mut nets = Vec::new();
        for e in entries {
            let s = e.as_ref().trim();
            let net = if s.contains('/') {
                IpNet::from_str(s).map_err(|_| "not a valid CIDR")?
            } else {
                IpNet::from(IpAddr::from_str(s).map_err(|_| "not a valid IP or CIDR")?)
            };
            if net.prefix_len() == 0 {
                return Err("a /0 network would trust every address");
            }
            nets.push(net.trunc());
        }
        Ok(Self { nets })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    #[must_use]
    pub fn networks(&self) -> &[IpNet] {
        &self.nets
    }

    #[must_use]
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.nets.iter().any(|n| n.contains(&ip))
    }

    /// Client IP for a request. `forwarded_for` holds every `X-Forwarded-For` header value in
    /// order. Untrusted peer: headers ignored. Trusted peer: the right-most entry that is not
    /// itself a trusted proxy; an unparsable entry stops the walk at the last trusted hop.
    #[must_use]
    pub fn client_ip(&self, peer: IpAddr, forwarded_for: &[&str]) -> IpAddr {
        let peer = peer.to_canonical();
        if !self.is_trusted(peer) {
            return peer;
        }
        let mut current = peer;
        let entries: Vec<&str> = forwarded_for
            .iter()
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .collect();
        for raw in entries.into_iter().rev() {
            let Ok(ip) = IpAddr::from_str(raw) else {
                return current;
            };
            let ip = ip.to_canonical();
            if self.is_trusted(ip) {
                current = ip;
            } else {
                return ip;
            }
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn proxies() -> TrustedProxies {
        TrustedProxies::parse(&["10.0.0.0/8", "::1"]).unwrap()
    }

    #[test]
    fn untrusted_peer_cannot_spoof() {
        assert_eq!(
            proxies().client_ip(ip("203.0.113.9"), &["1.2.3.4"]),
            ip("203.0.113.9")
        );
        assert_eq!(
            TrustedProxies::none().client_ip(ip("10.0.0.1"), &["1.2.3.4"]),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted() {
        let p = proxies();
        assert_eq!(
            p.client_ip(ip("10.0.0.1"), &["198.51.100.7"]),
            ip("198.51.100.7")
        );
        // client-prepended spoof stays left of the real client
        assert_eq!(
            p.client_ip(ip("10.0.0.1"), &["6.6.6.6, 198.51.100.7, 10.0.0.2"]),
            ip("198.51.100.7")
        );
        // several header lines behave like one comma list
        assert_eq!(
            p.client_ip(ip("10.0.0.1"), &["6.6.6.6", "198.51.100.7"]),
            ip("198.51.100.7")
        );
        assert_eq!(p.client_ip(ip("10.0.0.1"), &[]), ip("10.0.0.1"));
        assert_eq!(p.client_ip(ip("10.0.0.1"), &["10.0.0.9"]), ip("10.0.0.9"));
    }

    #[test]
    fn garbage_entry_stops_walk() {
        let p = proxies();
        assert_eq!(
            p.client_ip(ip("10.0.0.1"), &["1.1.1.1, nonsense"]),
            ip("10.0.0.1")
        );
        assert_eq!(p.client_ip(ip("10.0.0.1"), &["1.1.1.1, "]), ip("10.0.0.1"));
    }

    #[test]
    fn ipv4_mapped_ipv6_is_canonicalized() {
        let p = proxies();
        assert!(p.is_trusted(ip("::ffff:10.1.2.3")));
        assert_eq!(
            p.client_ip(ip("::ffff:10.0.0.1"), &["::ffff:198.51.100.7"]),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(TrustedProxies::parse(&["0.0.0.0/0"]).is_err());
        assert!(TrustedProxies::parse(&["::/0"]).is_err());
        assert!(TrustedProxies::parse(&["10.0.0.0/33"]).is_err());
        assert!(TrustedProxies::parse(&["not-an-ip"]).is_err());
        assert!(
            TrustedProxies::parse(&["192.168.1.5"])
                .unwrap()
                .is_trusted(ip("192.168.1.5"))
        );
    }
}
