//! Protocol messages (ADR-002). Every frame carries exactly one [`Message`] as JSON.

use std::collections::BTreeMap;

use msm_core::ErrorCode;
use msm_core::ids::{RequestId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Principal;

/// Protocol major version spoken by this build. Breaking changes bump it.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Oldest major this build still accepts (N-1 support window).
pub const PROTOCOL_MIN_SUPPORTED: u16 = 1;

/// Name and version of a client or daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareInfo {
    /// e.g. `msm-desktop`, `msmctl`, `msmd`.
    pub name: String,
    /// Semver of the build.
    pub version: String,
}

/// Error payload of a failed response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Stable code.
    pub code: ErrorCode,
    /// Translation key for the user-facing message.
    pub message_key: String,
    /// Developer detail (redacted).
    pub detail: String,
    /// Whether retrying later may help.
    pub retryable: bool,
}

impl From<msm_core::MsmError> for ApiError {
    fn from(e: msm_core::MsmError) -> Self {
        Self {
            message_key: e.code.message_key(),
            retryable: e.code.is_retryable(),
            code: e.code,
            detail: e.detail,
        }
    }
}

impl ApiError {
    /// Convenience constructor.
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        msm_core::MsmError::new(code, detail).into()
    }
}

/// Outcome of a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Success with a command-specific result.
    Ok(Value),
    /// Failure.
    Err(ApiError),
}

/// A protocol message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// Client → daemon, first message on a connection.
    Hello {
        /// Inclusive range of protocol majors the client speaks.
        proto: [u16; 2],
        /// Client identity.
        client: SoftwareInfo,
        /// Topic cursors to resume after a reconnect (`topic → last seen seq`).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        resume: BTreeMap<String, u64>,
    },
    /// Daemon → client, accepting the session.
    HelloOk {
        /// Negotiated protocol major.
        proto: u16,
        /// Daemon identity.
        daemon: SoftwareInfo,
        /// Additive feature flags.
        features: Vec<String>,
        /// Who the daemon thinks the client is.
        principal: Principal,
    },
    /// Client → daemon command.
    Req {
        /// Correlates the response.
        id: RequestId,
        /// Command name (see [`crate::commands::COMMANDS`]).
        cmd: String,
        /// Command parameters.
        #[serde(default)]
        params: Value,
        /// Makes retries of mutating commands safe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// Daemon → client response.
    Res {
        /// Request ID being answered.
        id: RequestId,
        /// Result.
        #[serde(flatten)]
        outcome: Outcome,
    },
    /// Daemon → client event on a subscribed topic.
    Event {
        /// Topic, e.g. `instance/<id>/console`.
        topic: String,
        /// Monotonic per-topic sequence number.
        seq: u64,
        /// Event payload.
        data: Value,
    },
    /// Daemon → client: events `from..=to` on `topic` were dropped or are no longer available.
    Gap {
        /// Topic.
        topic: String,
        /// First missing sequence number.
        from: u64,
        /// Last missing sequence number.
        to: u64,
    },
    /// Client → daemon: subscribe, optionally replaying history after `from_seq`.
    Sub {
        /// Topic.
        topic: String,
        /// Replay events with `seq > from_seq` if still buffered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_seq: Option<u64>,
    },
    /// Client → daemon: unsubscribe.
    Unsub {
        /// Topic.
        topic: String,
    },
    /// Client → daemon: cancel a running task.
    Cancel {
        /// Task to cancel.
        task_id: TaskId,
    },
    /// Keep-alive.
    Ping {
        /// Echoed in the pong.
        nonce: u64,
    },
    /// Keep-alive reply.
    Pong {
        /// Nonce from the ping.
        nonce: u64,
    },
    /// Either side: the connection is being closed with a reason.
    Bye {
        /// Why.
        error: ApiError,
    },
}

/// Picks the protocol major to use, preferring the newest both sides support.
///
/// # Errors
/// [`ErrorCode::IncompatibleProtocol`] if the ranges do not overlap.
pub fn negotiate(client: [u16; 2], daemon: [u16; 2]) -> Result<u16, ApiError> {
    let [cmin, cmax] = client;
    let [dmin, dmax] = daemon;
    let lo = cmin.max(dmin);
    let hi = cmax.min(dmax);
    if cmin <= cmax && lo <= hi {
        Ok(hi)
    } else {
        Err(ApiError::new(
            ErrorCode::IncompatibleProtocol,
            format!("client speaks {cmin}..={cmax}, daemon speaks {dmin}..={dmax}"),
        ))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn messages_round_trip_through_json() {
        let id = RequestId::new();
        let msgs = vec![
            Message::Hello {
                proto: [1, 1],
                client: SoftwareInfo {
                    name: "msm-desktop".into(),
                    version: "0.1.0".into(),
                },
                resume: BTreeMap::new(),
            },
            Message::Req {
                id,
                cmd: "instances.start".into(),
                params: json!({"instance": "x"}),
                idempotency_key: Some("k1".into()),
            },
            Message::Res {
                id,
                outcome: Outcome::Ok(json!({"state": "starting"})),
            },
            Message::Res {
                id,
                outcome: Outcome::Err(ApiError::new(ErrorCode::Forbidden, "missing control")),
            },
            Message::Event {
                topic: "instance/x/console".into(),
                seq: 7,
                data: json!("line"),
            },
            Message::Gap {
                topic: "instance/x/console".into(),
                from: 1,
                to: 6,
            },
            Message::Sub {
                topic: "t".into(),
                from_seq: Some(3),
            },
            Message::Ping { nonce: 9 },
        ];
        for m in msgs {
            let text = serde_json::to_string(&m).unwrap();
            let back: Message = serde_json::from_str(&text).unwrap();
            assert_eq!(back, m, "{text}");
        }
    }

    #[test]
    fn wire_shape_is_stable() {
        let id: RequestId = "01JDXK3Q5Z8M2N4P6R8T0V2X4Y".parse().unwrap();
        let m = Message::Res {
            id,
            outcome: Outcome::Ok(json!(1)),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"t":"res","id":"01JDXK3Q5Z8M2N4P6R8T0V2X4Y","ok":1}"#
        );
        let e = Message::Res {
            id,
            outcome: Outcome::Err(ApiError::new(ErrorCode::NotFound, "x")),
        };
        let v: Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["err"]["code"], "NOT_FOUND");
        assert_eq!(v["err"]["message_key"], "error.not_found");
    }

    #[test]
    fn negotiation() {
        assert_eq!(negotiate([1, 3], [2, 2]).unwrap(), 2);
        assert_eq!(negotiate([1, 1], [1, 2]).unwrap(), 1);
        let err = negotiate([3, 4], [1, 2]).unwrap_err();
        assert_eq!(err.code, ErrorCode::IncompatibleProtocol);
        assert!(negotiate([2, 1], [1, 2]).is_err());
    }
}
