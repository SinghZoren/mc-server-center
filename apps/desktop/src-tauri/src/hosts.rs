//! Hosts this desktop manages: the local PC plus paired remote daemons (REM-02/03).
//!
//! The desktop's identity (certificate + private key) and the list of paired hosts (address +
//! pinned fingerprint) live in the app's data directory, which is private to the Windows user.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use msm_api::{ApiError, Message, Outcome};
use msm_core::ErrorCode;
use msm_core::ids::RequestId;
use msm_transport::remote::{self, Identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::AppHandle;

use crate::daemon_link::{DaemonLink, Target, handshake};

/// A paired remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRecord {
    /// Stable ID used by the UI.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `host:port`.
    pub address: String,
    /// Pinned certificate fingerprint.
    pub fingerprint: String,
}

/// Registry of links, keyed by host ID (`local` for this PC).
pub struct Hosts {
    dir: PathBuf,
    links: Mutex<HashMap<String, DaemonLink>>,
}

fn err(code: ErrorCode, detail: impl Into<String>) -> ApiError {
    ApiError::new(code, detail)
}

impl Hosts {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            links: Mutex::new(HashMap::new()),
        }
    }

    fn records_path(&self) -> PathBuf {
        self.dir.join("hosts.json")
    }

    /// Saved remote hosts.
    pub fn records(&self) -> Vec<HostRecord> {
        std::fs::read_to_string(self.records_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, records: &[HostRecord]) -> Result<(), ApiError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        let text = serde_json::to_vec_pretty(records)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        msm_fsguard::write_atomic(&self.records_path(), &text)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))
    }

    /// This desktop's identity, created on first use.
    pub fn identity(&self) -> Result<Identity, ApiError> {
        let path = self.dir.join("identity.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(id) = Identity::from_json(&text)
        {
            return Ok(id);
        }
        let id = Identity::generate("desktop").map_err(|e| err(ErrorCode::Internal, e))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        msm_fsguard::write_atomic(&path, id.to_json().as_bytes())
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        Ok(id)
    }

    /// Starts links for the local daemon and every saved host.
    pub fn start_all(&self, app: &AppHandle) {
        let mut links = self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        links.insert(
            "local".into(),
            DaemonLink::start(app.clone(), "local".into(), Target::Local),
        );
        if let Ok(identity) = self.identity() {
            for r in self.records() {
                let target = Target::Remote {
                    address: r.address.clone(),
                    fingerprint: r.fingerprint.clone(),
                    identity: identity.clone(),
                };
                links.insert(
                    r.id.clone(),
                    DaemonLink::start(app.clone(), r.id.clone(), target),
                );
            }
        }
    }

    /// The link for a host.
    pub fn link(&self, host: &str) -> Result<DaemonLink, ApiError> {
        self.links
            .lock()
            .ok()
            .and_then(|l| l.get(host).cloned())
            .ok_or_else(|| err(ErrorCode::NotFound, format!("unknown host {host}")))
    }

    /// Pairs with a remote daemon using a one-time code shown on that host.
    pub async fn pair(
        &self,
        app: &AppHandle,
        address: &str,
        code: &str,
        name: &str,
    ) -> Result<HostRecord, ApiError> {
        let address = address.trim();
        if address.is_empty() || !address.contains(':') || address.chars().any(char::is_whitespace)
        {
            return Err(err(
                ErrorCode::InvalidArgument,
                "enter the host as address:port, for example 203.0.113.5:47625",
            ));
        }
        let identity = self.identity()?;
        let (tls, server_fp) = remote::connect(address, &identity, None)
            .await
            .map_err(|e| err(ErrorCode::UpstreamError, e))?;
        let mut conn = handshake(Box::new(tls))
            .await
            .map_err(|e| err(ErrorCode::UpstreamError, e))?;
        let id = RequestId::new();
        let proof = remote::client_proof(code, &server_fp, &identity.fingerprint());
        conn.send(&Message::Req {
            id,
            cmd: "pair.complete".into(),
            params: json!({"proof": proof, "name": "Desktop"}),
            idempotency_key: None,
        })
        .await
        .map_err(|e| err(ErrorCode::UpstreamError, e.to_string()))?;
        let result = loop {
            match conn.recv().await {
                Ok(Some(Message::Res { id: rid, outcome })) if rid == id => break outcome,
                Ok(Some(_)) => {}
                _ => {
                    return Err(err(
                        ErrorCode::UpstreamError,
                        "the host closed the connection",
                    ));
                }
            }
        };
        let value: Value = match result {
            Outcome::Ok(v) => v,
            Outcome::Err(e) => return Err(e),
        };
        let expected = remote::server_proof(code, &server_fp, &identity.fingerprint());
        if !remote::verify_proof(
            &expected,
            value["server_proof"].as_str().unwrap_or_default(),
        ) {
            return Err(err(
                ErrorCode::Unauthenticated,
                "the host could not prove it knows the code; it may not be the machine you think it is",
            ));
        }
        let record = HostRecord {
            id: format!("host-{}", RequestId::new()),
            name: if name.trim().is_empty() {
                address.to_owned()
            } else {
                name.trim().chars().take(64).collect()
            },
            address: address.to_owned(),
            fingerprint: server_fp,
        };
        let mut records = self.records();
        records.retain(|r| r.address != record.address);
        records.push(record.clone());
        self.save(&records)?;
        let target = Target::Remote {
            address: record.address.clone(),
            fingerprint: record.fingerprint.clone(),
            identity,
        };
        if let Ok(mut links) = self.links.lock() {
            links.insert(
                record.id.clone(),
                DaemonLink::start(app.clone(), record.id.clone(), target),
            );
        }
        Ok(record)
    }

    /// Forgets a remote host on this desktop (the host should also revoke this device).
    pub fn remove(&self, id: &str) -> Result<(), ApiError> {
        let mut records = self.records();
        records.retain(|r| r.id != id);
        self.save(&records)?;
        if let Ok(mut links) = self.links.lock() {
            links.remove(id);
        }
        Ok(())
    }
}
