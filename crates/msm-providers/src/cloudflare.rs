//! Cloudflare DNS (API v4) for linking a domain the user owns, and DNS-over-HTTPS lookups for
//! checking records at any provider.
//!
//! The API token is supplied by the user, needs only "Zone → DNS → Edit" for their zone, is
//! sent only to `api.cloudflare.com` as a bearer header, and is never logged or returned.
//! Records are created DNS-only (not proxied): Cloudflare's proxy does not carry Minecraft.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::http::{DownloadError, Downloader, HostPolicy};

const API: &str = "https://api.cloudflare.com/client/v4";
const MAX_BODY: u64 = 2 * 1024 * 1024;
/// Comment stored on records this app creates, so users can recognise them in Cloudflare.
pub const RECORD_COMMENT: &str = "Managed by MC Server Center";

fn api_policy() -> HostPolicy {
    HostPolicy::new(&["api.cloudflare.com"])
}

/// A Cloudflare zone (a domain in the user's account).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Zone {
    /// Zone ID.
    pub id: String,
    /// Domain name, e.g. `example.com`.
    pub name: String,
    /// `active`, `pending`, ...
    #[serde(default)]
    pub status: String,
}

/// A DNS record as Cloudflare reports it (only the fields we use).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// Record ID.
    pub id: String,
    /// `A`, `SRV`, ...
    #[serde(rename = "type")]
    pub kind: String,
    /// Full name.
    pub name: String,
    /// Value for simple records (A).
    #[serde(default)]
    pub content: String,
}

/// Errors talking to Cloudflare, with readable messages.
#[derive(Debug, thiserror::Error)]
pub enum CloudflareError {
    /// The token was rejected or lacks permission.
    #[error("Cloudflare refused the token: {0}")]
    Auth(String),
    /// Cloudflare reported an error.
    #[error("Cloudflare: {0}")]
    Api(String),
    /// Transport error.
    #[error(transparent)]
    Http(#[from] DownloadError),
}

#[derive(Deserialize)]
struct Envelope {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    #[serde(default)]
    result: Value,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

/// Cloudflare API client bound to one token.
pub struct Cloudflare {
    dl: Downloader,
    token: String,
}

impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cloudflare { token: <redacted> }")
    }
}

impl Cloudflare {
    /// A client using `token`.
    #[must_use]
    pub fn new(dl: Downloader, token: String) -> Self {
        Self { dl, token }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, CloudflareError> {
        let url =
            Url::parse(&format!("{API}{path}")).map_err(|e| CloudflareError::Api(e.to_string()))?;
        let (status, bytes) = self
            .dl
            .api_call(
                method,
                &url,
                &api_policy(),
                Some(&self.token),
                body,
                "application/json",
                MAX_BODY,
            )
            .await?;
        let env: Envelope = serde_json::from_slice(&bytes)
            .map_err(|_| CloudflareError::Api(format!("unexpected answer (HTTP {status})")))?;
        if env.success {
            return Ok(env.result);
        }
        let msg = env
            .errors
            .iter()
            .map(|e| format!("{} (code {})", e.message, e.code))
            .collect::<Vec<_>>()
            .join("; ");
        if status == 401
            || status == 403
            || env
                .errors
                .iter()
                .any(|e| matches!(e.code, 1000 | 9109 | 10000 | 6003 | 6111))
        {
            Err(CloudflareError::Auth(if msg.is_empty() {
                format!("HTTP {status}")
            } else {
                msg
            }))
        } else {
            Err(CloudflareError::Api(if msg.is_empty() {
                format!("HTTP {status}")
            } else {
                msg
            }))
        }
    }

    /// Checks that the token is valid and active.
    ///
    /// # Errors
    /// [`CloudflareError::Auth`] when it isn't.
    pub async fn verify(&self) -> Result<(), CloudflareError> {
        let r = self.call(Method::GET, "/user/tokens/verify", None).await?;
        if r["status"].as_str() == Some("active") {
            Ok(())
        } else {
            Err(CloudflareError::Auth(format!(
                "token status is {}",
                r["status"].as_str().unwrap_or("unknown")
            )))
        }
    }

    /// Zones (domains) the token can see.
    ///
    /// # Errors
    /// API errors.
    pub async fn zones(&self) -> Result<Vec<Zone>, CloudflareError> {
        let r = self.call(Method::GET, "/zones?per_page=50", None).await?;
        serde_json::from_value(r).map_err(|e| CloudflareError::Api(e.to_string()))
    }

    /// Records of `kind` named exactly `name` in `zone`.
    ///
    /// # Errors
    /// API errors.
    pub async fn find(
        &self,
        zone: &str,
        kind: &str,
        name: &str,
    ) -> Result<Vec<Record>, CloudflareError> {
        let mut url = Url::parse("https://x/").map_err(|e| CloudflareError::Api(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("type", kind)
            .append_pair("name", name);
        let q = url.query().unwrap_or_default().to_owned();
        let r = self
            .call(
                Method::GET,
                &format!("/zones/{}/dns_records?{q}", seg(zone)?),
                None,
            )
            .await?;
        serde_json::from_value(r).map_err(|e| CloudflareError::Api(e.to_string()))
    }

    /// Creates or updates the single record of `body.type` named `body.name`. Returns its ID.
    ///
    /// # Errors
    /// API errors.
    pub async fn upsert(&self, zone: &str, body: &Value) -> Result<String, CloudflareError> {
        let kind = body["type"].as_str().unwrap_or_default();
        let name = body["name"].as_str().unwrap_or_default();
        let existing = self.find(zone, kind, name).await?;
        let r = match existing.first() {
            Some(rec) => {
                self.call(
                    Method::PUT,
                    &format!("/zones/{}/dns_records/{}", seg(zone)?, seg(&rec.id)?),
                    Some(body),
                )
                .await?
            }
            None => {
                self.call(
                    Method::POST,
                    &format!("/zones/{}/dns_records", seg(zone)?),
                    Some(body),
                )
                .await?
            }
        };
        r["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CloudflareError::Api("record has no id".into()))
    }

    /// Deletes a record (missing records are fine).
    ///
    /// # Errors
    /// API errors other than "not found".
    pub async fn delete(&self, zone: &str, id: &str) -> Result<(), CloudflareError> {
        match self
            .call(
                Method::DELETE,
                &format!("/zones/{}/dns_records/{}", seg(zone)?, seg(id)?),
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(CloudflareError::Api(m))
                if m.contains("81044") || m.to_lowercase().contains("not found") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// IDs go into URL paths: Cloudflare IDs are hex; refuse anything else.
fn seg(id: &str) -> Result<&str, CloudflareError> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err(CloudflareError::Api("invalid id".into()))
    }
}

/// The A record body for `name` → `ip` (DNS-only, short TTL so IP changes spread quickly).
#[must_use]
pub fn a_record(name: &str, ip: std::net::Ipv4Addr) -> Value {
    json!({ "type": "A", "name": name, "content": ip.to_string(), "ttl": 60, "proxied": false, "comment": RECORD_COMMENT })
}

/// The SRV record that lets players type just `name` for a server on a non-default `port`.
#[must_use]
pub fn srv_record(name: &str, port: u16) -> Value {
    json!({
        "type": "SRV",
        "name": format!("_minecraft._tcp.{name}"),
        "data": { "priority": 0, "weight": 5, "port": port, "target": name },
        "ttl": 60,
        "comment": RECORD_COMMENT,
    })
}

/// Looks up `name`'s records of `kind` (`A` or `SRV`) through Cloudflare's public DNS-over-HTTPS
/// resolver. Returns the record data strings (for SRV: `"priority weight port target."`).
///
/// # Errors
/// Transport errors or an unparseable answer.
pub async fn lookup(dl: &Downloader, name: &str, kind: &str) -> Result<Vec<String>, DownloadError> {
    let mut url = Url::parse("https://cloudflare-dns.com/dns-query")
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("name", name)
        .append_pair("type", kind);
    let policy = HostPolicy::new(&["cloudflare-dns.com"]);
    let (status, bytes) = dl
        .api_call(
            Method::GET,
            &url,
            &policy,
            None,
            None,
            "application/dns-json",
            256 * 1024,
        )
        .await?;
    if status != 200 {
        return Err(DownloadError::Status(status));
    }
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let want = if kind == "SRV" { 33 } else { 1 };
    Ok(v["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"].as_u64() == Some(want))
        .filter_map(|a| a["data"].as_str().map(str::to_owned))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_dns_only_and_marked() {
        let a = a_record("mc.example.com", "203.0.113.5".parse().unwrap());
        assert_eq!(a["proxied"], false);
        assert_eq!(a["content"], "203.0.113.5");
        assert_eq!(a["comment"], RECORD_COMMENT);
        let s = srv_record("mc.example.com", 25570);
        assert_eq!(s["name"], "_minecraft._tcp.mc.example.com");
        assert_eq!(s["data"]["port"], 25570);
        assert_eq!(s["data"]["target"], "mc.example.com");
    }

    #[test]
    fn ids_cannot_inject_paths() {
        assert!(seg("023e105f4ecef8ad9ca31a8372d0c353").is_ok());
        assert!(seg("../zones").is_err());
        assert!(seg("a/b").is_err());
        assert!(seg("").is_err());
    }

    #[test]
    fn debug_hides_the_token() {
        let c = Cloudflare::new(Downloader::new("a", "b"), "secret-token".into());
        assert!(!format!("{c:?}").contains("secret"));
    }
}
