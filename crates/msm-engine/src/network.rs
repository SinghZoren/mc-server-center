//! Local network facts for the "how friends join" panel (NET-01).

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// The IPv4 address this machine uses on its local network, found by asking the OS which source
/// address it would use to reach a public address. **No packet is sent**: `connect` on a UDP
/// socket only selects a route.
#[must_use]
pub fn primary_lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?; // TEST-NET-1, never routed publicly
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Whether an address is in a private (RFC 1918) or carrier-grade NAT (RFC 6598) range.
#[must_use]
pub fn classify(ip: Ipv4Addr) -> &'static str {
    let o = ip.octets();
    if ip.is_private() {
        "private"
    } else if o[0] == 100 && (64..128).contains(&o[1]) {
        "cgnat"
    } else if ip.is_link_local() {
        "link_local"
    } else {
        "public"
    }
}

/// Public IPv4 as seen from the internet, from up to two independent sources (NET-03).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PublicIp {
    /// The address, if any source answered.
    pub ip: Option<Ipv4Addr>,
    /// Sources that answered with this address.
    pub sources: Vec<&'static str>,
    /// Whether two sources disagreed (then `ip` is not trusted).
    pub disagreement: bool,
}

fn echo_policy() -> msm_providers::http::HostPolicy {
    msm_providers::http::HostPolicy::new(&["cloudflare.com", "api.ipify.org"])
}

async fn fetch_text(dl: &msm_providers::http::Downloader, url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        dl.get_bytes(&url, &echo_policy(), 16 * 1024),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8(bytes).ok()
}

/// Parses Cloudflare's `/cdn-cgi/trace` output (`ip=1.2.3.4` line).
#[must_use]
pub fn parse_trace(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .find_map(|l| l.strip_prefix("ip="))
        .and_then(|v| v.trim().parse().ok())
}

/// Asks Cloudflare's trace endpoint and ipify for this network's public IPv4.
pub async fn public_ipv4(dl: &msm_providers::http::Downloader) -> PublicIp {
    let (cf, ipify) = tokio::join!(
        fetch_text(dl, "https://cloudflare.com/cdn-cgi/trace"),
        fetch_text(dl, "https://api.ipify.org")
    );
    let a = cf.as_deref().and_then(parse_trace);
    let b = ipify
        .as_deref()
        .and_then(|t| t.trim().parse::<Ipv4Addr>().ok());
    match (a, b) {
        (Some(x), Some(y)) if x == y => PublicIp {
            ip: Some(x),
            sources: vec!["cloudflare.com", "api.ipify.org"],
            disagreement: false,
        },
        (Some(_), Some(_)) => PublicIp {
            ip: None,
            sources: vec![],
            disagreement: true,
        },
        (Some(x), None) => PublicIp {
            ip: Some(x),
            sources: vec!["cloudflare.com"],
            disagreement: false,
        },
        (None, Some(y)) => PublicIp {
            ip: Some(y),
            sources: vec!["api.ipify.org"],
            disagreement: false,
        },
        (None, None) => PublicIp {
            ip: None,
            sources: vec![],
            disagreement: false,
        },
    }
}

fn search_options(secs: u64) -> igd_next::SearchOptions {
    igd_next::SearchOptions {
        timeout: Some(std::time::Duration::from_secs(secs)),
        ..Default::default()
    }
}

/// Finds a UPnP router and returns its WAN address (None if there is no UPnP router).
pub async fn router_wan_ip() -> Option<IpAddr> {
    let gw = igd_next::aio::tokio::search_gateway(search_options(3))
        .await
        .ok()?;
    gw.get_external_ip().await.ok()
}

/// How likely direct connections from the internet are to work (NET-03). Worded as likelihood:
/// routers and ISPs can't be inspected completely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The router has the public address: port forwarding should work.
    DirectPossible,
    /// The router's WAN address is shared or differs: the ISP probably uses carrier-grade NAT.
    CgnatLikely,
    /// The router's WAN address is private: another router/NAT is upstream.
    DoubleNatLikely,
    /// No UPnP router information; can't tell.
    Unknown,
}

/// Combines what we observed into a verdict.
#[must_use]
pub fn assess(public: Option<Ipv4Addr>, router_wan: Option<IpAddr>) -> Verdict {
    match (public, router_wan) {
        (_, Some(IpAddr::V4(wan))) if classify(wan) == "cgnat" => Verdict::CgnatLikely,
        (_, Some(IpAddr::V4(wan))) if classify(wan) == "private" => Verdict::DoubleNatLikely,
        (Some(p), Some(IpAddr::V4(wan))) if p == wan => Verdict::DirectPossible,
        (Some(_), Some(IpAddr::V4(_))) => Verdict::CgnatLikely,
        _ => Verdict::Unknown,
    }
}

/// Opens `port` (TCP) on the UPnP router towards `lan_ip`. Only call after the user agreed.
///
/// # Errors
/// A readable reason if there is no UPnP router or it refused.
pub async fn upnp_open(port: u16, lan_ip: Ipv4Addr) -> Result<(), String> {
    let gw = igd_next::aio::tokio::search_gateway(search_options(4))
        .await
        .map_err(|_| {
            "no router with UPnP was found (it may be turned off in the router settings)".to_owned()
        })?;
    gw.add_port(
        igd_next::PortMappingProtocol::TCP,
        port,
        std::net::SocketAddr::new(IpAddr::V4(lan_ip), port),
        0,
        "Minecraft server (MC Server Center)",
    )
    .await
    .map_err(|e| format!("the router refused to open the port: {e}"))
}

/// Removes the UPnP mapping for `port`.
///
/// # Errors
/// A readable reason.
pub async fn upnp_close(port: u16) -> Result<(), String> {
    let gw = igd_next::aio::tokio::search_gateway(search_options(4))
        .await
        .map_err(|_| "no router with UPnP was found".to_owned())?;
    gw.remove_port(igd_next::PortMappingProtocol::TCP, port)
        .await
        .map_err(|e| format!("could not remove the mapping: {e}"))
}

/// Result of an outside-in check (NET-04). Never `pass` without an external source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExternalCheck {
    /// `pass`, `fail` or `not_tested`.
    pub status: &'static str,
    /// Who checked.
    pub source: &'static str,
    /// Caveats shown with the result.
    pub note: String,
}

/// Asks mcsrvstat.us (a third-party service; only after the user agreed) whether a Minecraft
/// server answers at `ip:port`. The service may cache results for up to 5 minutes.
pub async fn external_check(
    dl: &msm_providers::http::Downloader,
    ip: Ipv4Addr,
    port: u16,
) -> ExternalCheck {
    let note =
        "Checked by mcsrvstat.us from the internet; results can be up to 5 minutes old.".to_owned();
    let Ok(url) = url::Url::parse(&format!("https://api.mcsrvstat.us/3/{ip}:{port}")) else {
        return ExternalCheck {
            status: "not_tested",
            source: "mcsrvstat.us",
            note,
        };
    };
    let policy = msm_providers::http::HostPolicy::new(&["api.mcsrvstat.us"]);
    match dl
        .get_json::<serde_json::Value>(&url, &policy, 256 * 1024)
        .await
    {
        Ok(v) if v["online"].as_bool() == Some(true) => ExternalCheck {
            status: "pass",
            source: "mcsrvstat.us",
            note,
        },
        Ok(_) => ExternalCheck {
            status: "fail",
            source: "mcsrvstat.us",
            note,
        },
        Err(e) => ExternalCheck {
            status: "not_tested",
            source: "mcsrvstat.us",
            note: format!("The checker could not be reached: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify(Ipv4Addr::new(192, 168, 1, 20)), "private");
        assert_eq!(classify(Ipv4Addr::new(10, 0, 0, 5)), "private");
        assert_eq!(classify(Ipv4Addr::new(100, 72, 1, 1)), "cgnat");
        assert_eq!(classify(Ipv4Addr::new(169, 254, 3, 3)), "link_local");
        assert_eq!(classify(Ipv4Addr::new(203, 0, 113, 9)), "public");
    }

    #[test]
    fn trace_parsing_and_verdicts() {
        assert_eq!(
            parse_trace("fl=1\nip=203.0.113.9\nts=1"),
            Some(Ipv4Addr::new(203, 0, 113, 9))
        );
        assert_eq!(parse_trace("nothing"), None);
        let public = Some(Ipv4Addr::new(203, 0, 113, 9));
        let wan = |a, b, c, d| Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
        assert_eq!(assess(public, wan(203, 0, 113, 9)), Verdict::DirectPossible);
        assert_eq!(assess(public, wan(100, 72, 1, 1)), Verdict::CgnatLikely);
        assert_eq!(
            assess(public, wan(192, 168, 0, 1)),
            Verdict::DoubleNatLikely
        );
        assert_eq!(assess(public, wan(198, 51, 100, 7)), Verdict::CgnatLikely);
        assert_eq!(assess(public, None), Verdict::Unknown);
    }

    #[test]
    fn primary_address_is_not_loopback() {
        if let Some(ip) = primary_lan_ipv4() {
            assert!(!ip.is_loopback());
        }
    }
}
