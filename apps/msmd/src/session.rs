//! One client connection: handshake, then requests, subscriptions and events.

use std::sync::Arc;

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::envelope::negotiate;
use msm_api::{
    ApiError, Message, Outcome, PROTOCOL_MAJOR, PROTOCOL_MIN_SUPPORTED, Principal, SoftwareInfo,
};
use msm_core::ErrorCode;
use msm_transport::FramedConn;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::daemon::Daemon;

/// Serves one connection until it closes.
pub async fn serve<S>(
    daemon: Arc<Daemon>,
    stream: S,
    principal: Principal,
    cancel: Option<tokio_util::sync::CancellationToken>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut conn = FramedConn::new(stream, MAX_CONTROL_FRAME);

    // Handshake: the first message must be Hello.
    let Ok(Some(Message::Hello { proto, client, .. })) = conn.recv().await else {
        let _ = conn
            .send(&Message::Bye {
                error: ApiError::new(ErrorCode::InvalidArgument, "expected hello"),
            })
            .await;
        return;
    };
    let negotiated = match negotiate(proto, [PROTOCOL_MIN_SUPPORTED, PROTOCOL_MAJOR]) {
        Ok(v) => v,
        Err(error) => {
            let _ = conn.send(&Message::Bye { error }).await;
            return;
        }
    };
    let relay_data = client.name == crate::relay::DATA_CLIENT;
    if conn
        .send(&Message::HelloOk {
            proto: negotiated,
            daemon: SoftwareInfo {
                name: "msmd".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            features: vec!["console".into(), "tasks".into()],
            principal: principal.clone(),
        })
        .await
        .is_err()
    {
        return;
    }

    if relay_data {
        crate::relay::serve_data(daemon, conn, principal).await;
        return;
    }

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let cancel = cancel.unwrap_or_default();
    let limited = matches!(principal, Principal::Device { .. });
    let mut window_start = std::time::Instant::now();
    let mut window_count = 0u32;
    let mut forwarders: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();

    loop {
        tokio::select! {
            incoming = conn.recv() => {
                let msg = match incoming {
                    Ok(Some(m)) => m,
                    Ok(None) | Err(_) => break,
                };
                match msg {
                    Message::Req { id, cmd, params, .. } if limited && {
                        // Remote devices: at most 50 requests per 10 seconds (T11).
                        if window_start.elapsed() > std::time::Duration::from_secs(10) {
                            window_start = std::time::Instant::now();
                            window_count = 0;
                        }
                        window_count += 1;
                        window_count > 50
                    } => {
                        let _ = out_tx.send(Message::Res { id, outcome: Outcome::Err(ApiError::new(ErrorCode::RateLimited, "too many requests; slow down")) }).await;
                        let _ = cmd;
                        let _ = params;
                    }
                    Message::Req { id, cmd, params, .. } => {
                        let d = daemon.clone();
                        let tx = out_tx.clone();
                        let principal = principal.clone();
                        tokio::spawn(async move {
                            let outcome = match d.dispatch(&principal, &cmd, &params).await {
                                Ok(v) => Outcome::Ok(v),
                                Err(e) => Outcome::Err(e),
                            };
                            let _ = tx.send(Message::Res { id, outcome }).await;
                        });
                    }
                    Message::Sub { topic, from_seq } => {
                        // Relay topics belong to one device; everything else needs View.
                        let allowed = match (&principal, topic.strip_prefix("relay/")) {
                            (Principal::Device { id, scopes }, Some(dev)) => dev == id.to_string() && scopes.contains(&msm_api::Capability::Relay),
                            (Principal::Device { .. }, None) => principal.has(msm_api::Capability::View),
                            (_, Some(_)) => false,
                            (_, None) => principal.has(msm_api::Capability::View),
                        };
                        if !allowed || !valid_topic(&topic) {
                            continue;
                        }
                        if forwarders.iter().any(|(t, _)| *t == topic) {
                            continue;
                        }
                        if let Some(sub) = daemon.events.subscribe(&topic, from_seq) {
                            let tx = out_tx.clone();
                            let t = topic.clone();
                            forwarders.push((topic, tokio::spawn(forward(t, sub, tx))));
                        }
                    }
                    Message::Unsub { topic } => {
                        forwarders.retain(|(t, h)| {
                            if *t == topic { h.abort(); false } else { true }
                        });
                    }
                    Message::Ping { nonce } => {
                        let _ = out_tx.send(Message::Pong { nonce }).await;
                    }
                    Message::Cancel { task_id } => {
                        let p = serde_json::json!({ "task": task_id });
                        let _ = daemon.dispatch(&principal, "tasks.cancel", &p).await;
                    }
                    _ => {}
                }
            }
            () = cancel.cancelled() => {
                let _ = conn.send(&Message::Bye { error: ApiError::new(ErrorCode::Unauthenticated, "access for this device was revoked") }).await;
                break;
            }
            Some(out) = out_rx.recv() => {
                if conn.send(&out).await.is_err() {
                    break;
                }
            }
        }
    }
    for (_, h) in forwarders {
        h.abort();
    }
}

fn valid_topic(topic: &str) -> bool {
    topic.len() <= 128
        && topic.split('/').all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

async fn forward(topic: String, sub: crate::events::Subscription, tx: mpsc::Sender<Message>) {
    if let Some((from, to)) = sub.gap {
        let _ = tx
            .send(Message::Gap {
                topic: topic.clone(),
                from,
                to,
            })
            .await;
    }
    let mut last = 0;
    for (seq, data) in sub.backlog {
        last = seq;
        if tx
            .send(Message::Event {
                topic: topic.clone(),
                seq,
                data,
            })
            .await
            .is_err()
        {
            return;
        }
    }
    let mut rx = sub.rx;
    loop {
        match rx.recv().await {
            Ok((seq, data)) => {
                if seq <= last {
                    continue; // already sent from the backlog
                }
                last = seq;
                if tx
                    .send(Message::Event {
                        topic: topic.clone(),
                        seq,
                        data,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let _ = tx
                    .send(Message::Gap {
                        topic: topic.clone(),
                        from: last + 1,
                        to: last + n,
                    })
                    .await;
                last += n;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}
