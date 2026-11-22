//! Linking a domain the user owns to a server (DNS-01..04): hostname validation, which records
//! are needed, and checking what DNS currently says. Pure functions; the daemon does the I/O.
//!
//! * An **A record** points the name at this machine's public IPv4.
//! * When the server isn't on Minecraft's default port 25565, an **SRV record**
//!   (`_minecraft._tcp.<name>`) lets players type just the name, without `:port`.

use std::net::Ipv4Addr;

use serde::Serialize;

/// Minecraft's default port; no SRV record is needed for it.
pub const DEFAULT_PORT: u16 = 25565;

/// Normalises what the user typed into a hostname (`Play.Example.com.` → `play.example.com`).
///
/// # Errors
/// A readable reason when it isn't a usable hostname.
pub fn normalize_hostname(input: &str) -> Result<String, String> {
    let mut h = input.trim().to_ascii_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = h.strip_prefix(prefix) {
            h = rest.to_owned();
        }
    }
    let h = h.trim_end_matches('/').trim_end_matches('.').to_owned();
    if h.contains(':') || h.contains('/') {
        return Err(
            "enter just the name, without a port or path (for example play.example.com)".into(),
        );
    }
    if h.is_empty() || h.len() > 253 {
        return Err("enter a domain name, for example play.example.com".into());
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2 {
        return Err("enter a full domain name, for example play.example.com".into());
    }
    for l in &labels {
        let ok = !l.is_empty()
            && l.len() <= 63
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-');
        if !ok {
            return Err(format!("\"{l}\" isn't a valid part of a domain name"));
        }
    }
    if labels
        .last()
        .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()))
    {
        return Err("that looks like an IP address; enter a domain name instead".into());
    }
    Ok(h)
}

/// The zone (domain) among `zones` that contains `host`: the longest matching suffix.
#[must_use]
pub fn zone_for<'a>(host: &str, zones: &'a [String]) -> Option<&'a String> {
    zones
        .iter()
        .filter(|z| host == z.as_str() || host.ends_with(&format!(".{z}")))
        .max_by_key(|z| z.len())
}

/// What players type to join.
#[must_use]
pub fn share_address(host: &str, port: u16) -> String {
    // With an SRV record the port is found automatically, but showing it is harmless and keeps
    // working if the SRV record is missing; only omit it when it's the default.
    if port == DEFAULT_PORT {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

/// One DNS record to create at the user's DNS provider.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PlannedRecord {
    /// `A` or `SRV`.
    pub kind: &'static str,
    /// Full record name.
    pub name: String,
    /// What to enter as the value.
    pub value: String,
    /// Plain-language hint.
    pub note: String,
}

/// The records needed for `host` → `ip` with the server on `port`.
#[must_use]
pub fn planned_records(host: &str, ip: Ipv4Addr, port: u16) -> Vec<PlannedRecord> {
    let mut out = vec![PlannedRecord {
        kind: "A",
        name: host.to_owned(),
        value: ip.to_string(),
        note: "Points the name at this machine. Turn off any proxy/CDN option for it (Cloudflare: \"DNS only\").".into(),
    }];
    if port != DEFAULT_PORT {
        out.push(PlannedRecord {
            kind: "SRV",
            name: format!("_minecraft._tcp.{host}"),
            value: format!("0 5 {port} {host}"),
            note: format!(
                "Lets players type just {host} without :{port}. Fields: priority 0, weight 5, port {port}, target {host}."
            ),
        });
    }
    out
}

/// The outcome of checking DNS for a link.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DnsCheck {
    /// A record values found.
    pub a_found: Vec<String>,
    /// Whether one of them is the expected public IP.
    pub a_ok: bool,
    /// Whether an SRV record is needed (non-default port).
    pub srv_needed: bool,
    /// Whether a matching SRV record was found (always true when not needed).
    pub srv_ok: bool,
    /// Plain-language summary.
    pub summary: String,
}

/// Compares DNS answers with what the link should resolve to.
#[must_use]
pub fn check(
    host: &str,
    expected: Option<Ipv4Addr>,
    a_found: Vec<String>,
    srv_found: &[String],
    port: u16,
) -> DnsCheck {
    let a_ok = expected.is_some_and(|ip| a_found.iter().any(|v| v.trim() == ip.to_string()));
    let srv_needed = port != DEFAULT_PORT;
    // SRV data looks like "0 5 25570 play.example.com."
    let srv_ok = !srv_needed
        || srv_found.iter().any(|d| {
            let parts: Vec<&str> = d.split_whitespace().collect();
            parts.len() == 4
                && parts[2] == port.to_string()
                && parts[3].trim_end_matches('.') == host
        });
    let summary = match (a_found.is_empty(), a_ok, srv_ok) {
        (true, _, _) => format!(
            "{host} doesn't point anywhere yet. New records can take a few minutes to appear."
        ),
        (false, false, _) => match expected {
            Some(ip) => format!(
                "{host} points to {}, not to this machine ({ip}).",
                a_found.join(", ")
            ),
            None => format!(
                "{host} points to {}, but this machine's public address couldn't be found to compare.",
                a_found.join(", ")
            ),
        },
        (false, true, false) => format!(
            "{host} points here, but the SRV record for port {port} is missing, so players must type {host}:{port}."
        ),
        (false, true, true) => format!(
            "{host} points to this machine. Friends can join with {}.",
            share_address(host, port)
        ),
    };
    DnsCheck {
        a_found,
        a_ok,
        srv_needed,
        srv_ok,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_are_normalised_and_validated() {
        assert_eq!(
            normalize_hostname(" Play.Example.COM. ").unwrap(),
            "play.example.com"
        );
        assert_eq!(
            normalize_hostname("https://mc.example.com/").unwrap(),
            "mc.example.com"
        );
        assert!(normalize_hostname("example").is_err());
        assert!(normalize_hostname("play.example.com:25565").is_err());
        assert!(normalize_hostname("-bad.example.com").is_err());
        assert!(normalize_hostname("bad_name.example.com").is_err());
        assert!(normalize_hostname("203.0.113.5").is_err());
        assert!(normalize_hostname(&format!("{}.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn longest_zone_wins() {
        let zones = vec![
            "example.com".to_owned(),
            "mc.example.com".to_owned(),
            "other.org".to_owned(),
        ];
        assert_eq!(
            zone_for("play.mc.example.com", &zones).unwrap(),
            "mc.example.com"
        );
        assert_eq!(zone_for("example.com", &zones).unwrap(), "example.com");
        assert!(zone_for("notexample.com", &zones).is_none());
    }

    #[test]
    fn srv_only_for_non_default_ports() {
        let ip = "203.0.113.5".parse().unwrap();
        assert_eq!(planned_records("mc.example.com", ip, 25565).len(), 1);
        let r = planned_records("mc.example.com", ip, 25570);
        assert_eq!(r[1].name, "_minecraft._tcp.mc.example.com");
        assert_eq!(r[1].value, "0 5 25570 mc.example.com");
        assert_eq!(share_address("mc.example.com", 25565), "mc.example.com");
        assert_eq!(
            share_address("mc.example.com", 25570),
            "mc.example.com:25570"
        );
    }

    #[test]
    fn checks_compare_with_expected_records() {
        let ip: Ipv4Addr = "203.0.113.5".parse().unwrap();
        let none = check("mc.example.com", Some(ip), vec![], &[], 25565);
        assert!(!none.a_ok && none.summary.contains("doesn't point anywhere"));
        let wrong = check(
            "mc.example.com",
            Some(ip),
            vec!["198.51.100.1".into()],
            &[],
            25565,
        );
        assert!(!wrong.a_ok && wrong.summary.contains("not to this machine"));
        let good = check(
            "mc.example.com",
            Some(ip),
            vec!["203.0.113.5".into()],
            &[],
            25565,
        );
        assert!(good.a_ok && good.srv_ok);
        let no_srv = check(
            "mc.example.com",
            Some(ip),
            vec!["203.0.113.5".into()],
            &[],
            25570,
        );
        assert!(no_srv.a_ok && !no_srv.srv_ok);
        let srv = check(
            "mc.example.com",
            Some(ip),
            vec!["203.0.113.5".into()],
            &["0 5 25570 mc.example.com.".into()],
            25570,
        );
        assert!(srv.srv_ok && srv.summary.contains("mc.example.com:25570"));
    }
}
