//! `msmctl` — a small command-line client for the local daemon. Useful for administration,
//! headless Linux hosts, and end-to-end tests.
//!
//! ```text
//! msmctl [--data-dir DIR] <command>
//!   ping | info | versions | java | list
//!   create <name> <mc_version> <port> <memory_mb> [vanilla|paper|fabric]
//!   eula <instance>                 accept the Minecraft EULA (https://aka.ms/MinecraftEULA)
//!   start <instance> | stop <instance> | kill <instance>
//!   send <instance> <command...>    run one console command
//!   console <instance>              stream the console; type commands to send them
//!   players <instance> | diagnose <instance> | backups <instance> | backup <instance> [full|world]
//!   remote-on [port] | pair-code [name] | devices | revoke <device>   (on the host)
//!   remote-pair <host:port> <CODE> [name], then: msmctl --host <host:port> <command>
//!   call <api.command> [json]       raw API call; prints {"ok":..} or {"err":..}
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{Message, Outcome, PROTOCOL_MAJOR, PROTOCOL_MIN_SUPPORTED, SoftwareInfo};
use msm_core::ids::RequestId;
use msm_storage::layout::DataDir;
use msm_transport::FramedConn;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};

type Conn = FramedConn<Box<dyn Stream>>;
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

async fn connect(data_dir: Option<PathBuf>) -> Conn {
    let data = match data_dir {
        Some(d) => DataDir::at(d),
        None => DataDir::default_for_os().unwrap_or_else(|e| fail(e)),
    };
    let endpoint = std::fs::read_to_string(data.run().join("endpoint")).unwrap_or_else(|_| {
        fail("daemon is not running (no endpoint file); start it with `msmd serve`")
    });
    #[cfg(windows)]
    let stream: Box<dyn Stream> = Box::new(
        msm_transport::windows_pipe::connect(endpoint.trim())
            .await
            .unwrap_or_else(|e| fail(format!("cannot connect to daemon: {e}"))),
    );
    #[cfg(unix)]
    let stream: Box<dyn Stream> = Box::new(
        msm_transport::unix_socket::connect(std::path::Path::new(endpoint.trim()))
            .await
            .unwrap_or_else(|e| fail(format!("cannot connect to daemon: {e}"))),
    );
    let mut conn = FramedConn::new(stream, MAX_CONTROL_FRAME);
    let hello = Message::Hello {
        proto: [PROTOCOL_MIN_SUPPORTED, PROTOCOL_MAJOR],
        client: SoftwareInfo {
            name: "msmctl".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume: BTreeMap::new(),
    };
    conn.send(&hello).await.unwrap_or_else(|e| fail(e));
    match conn.recv().await {
        Ok(Some(Message::HelloOk { .. })) => conn,
        Ok(Some(Message::Bye { error })) => fail(format!("{}: {}", error.code, error.detail)),
        other => fail(format!("unexpected handshake reply: {other:?}")),
    }
}

async fn request(conn: &mut Conn, cmd: &str, params: Value) -> Value {
    let id = RequestId::new();
    conn.send(&Message::Req {
        id,
        cmd: cmd.into(),
        params,
        idempotency_key: None,
    })
    .await
    .unwrap_or_else(|e| fail(e));
    loop {
        match conn.recv().await {
            Ok(Some(Message::Res { id: rid, outcome })) if rid == id => {
                return match outcome {
                    Outcome::Ok(v) => v,
                    Outcome::Err(e) => fail(format!("{}: {}", e.code, e.detail)),
                };
            }
            Ok(Some(_)) => {}
            Ok(None) => fail("daemon closed the connection"),
            Err(e) => fail(e),
        }
    }
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir = None;
    let mut host = None;
    loop {
        match args.first().map(String::as_str) {
            Some("--data-dir") if args.len() >= 2 => {
                data_dir = Some(PathBuf::from(args.remove(1)));
                args.remove(0);
            }
            Some("--host") if args.len() >= 2 => {
                host = Some(args.remove(1));
                args.remove(0);
            }
            Some("--data-dir" | "--host") => fail("option needs a value"),
            _ => break,
        }
    }
    let client_dir = data_dir
        .clone()
        .map(DataDir::at)
        .or_else(|| DataDir::default_for_os().ok())
        .map(|d| d.root().join("clients"))
        .unwrap_or_else(|| PathBuf::from("msm-clients"));
    if args.first().map(String::as_str) == Some("remote-pair") {
        remote_pair(
            &client_dir,
            args.get(1).cloned(),
            args.get(2).cloned(),
            args.get(3).cloned(),
        )
        .await;
        return;
    }
    let arg = |i: usize| {
        args.get(i)
            .cloned()
            .unwrap_or_else(|| fail("missing argument; see `msmctl help`"))
    };
    let cmd = args.first().cloned().unwrap_or_else(|| "help".into());
    if cmd == "help" {
        println!(
            "{}",
            include_str!("main.rs")
                .lines()
                .take(16)
                .skip(3)
                .map(|l| l.trim_start_matches("//! "))
                .collect::<Vec<_>>()
                .join("\n")
        );
        return;
    }
    let mut conn = match host {
        Some(h) => connect_remote(&client_dir, &h).await,
        None => connect(data_dir).await,
    };
    let instance = |i: usize| json!({ "instance": arg(i) });
    match cmd.as_str() {
        "call" => {
            let params = match args.get(2) {
                Some(t) => serde_json::from_str(t)
                    .unwrap_or_else(|e| fail(format!("params are not JSON: {e}"))),
                None => Value::Null,
            };
            let id = RequestId::new();
            conn.send(&Message::Req {
                id,
                cmd: arg(1),
                params,
                idempotency_key: None,
            })
            .await
            .unwrap_or_else(|e| fail(e));
            loop {
                match conn.recv().await {
                    Ok(Some(Message::Res { id: rid, outcome })) if rid == id => {
                        let out = match outcome {
                            Outcome::Ok(v) => json!({ "ok": v }),
                            Outcome::Err(e) => json!({ "err": e }),
                        };
                        println!("{out}");
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => fail("daemon closed the connection"),
                    Err(e) => fail(e),
                }
            }
        }
        "ping" => print(&request(&mut conn, "system.ping", Value::Null).await),
        "info" => print(&request(&mut conn, "system.info", Value::Null).await),
        "versions" => {
            let software = args.get(1).cloned().unwrap_or_else(|| "vanilla".into());
            print(&request(&mut conn, "catalog.list", json!({"software": software})).await);
        }
        "java" => print(&request(&mut conn, "java.list", Value::Null).await),
        "list" => print(&request(&mut conn, "instances.list", Value::Null).await),
        "eula" => print(
            &request(
                &mut conn,
                "instances.accept_eula",
                json!({"instance": arg(1), "accept": true}),
            )
            .await,
        ),
        "start" => print(&request(&mut conn, "instances.start", instance(1)).await),
        "stop" => print(&request(&mut conn, "instances.stop", instance(1)).await),
        "kill" => print(
            &request(
                &mut conn,
                "instances.kill",
                json!({"instance": arg(1), "confirm": true}),
            )
            .await,
        ),
        "players" => print(&request(&mut conn, "players.list", instance(1)).await),
        "diagnose" => print(&request(&mut conn, "diagnostics.list", instance(1)).await),
        "packs" => {
            let q = args[1..].join(" ");
            let r = request(&mut conn, "modpacks.search", json!({"query": q})).await;
            for h in r["hits"].as_array().into_iter().flatten() {
                println!(
                    "{:<10} {:<34} {:>10} downloads",
                    h["project_id"].as_str().unwrap_or(""),
                    h["title"].as_str().unwrap_or(""),
                    h["downloads"]
                );
            }
        }
        "pack-install" => {
            conn.send(&Message::Sub {
                topic: "tasks".into(),
                from_seq: None,
            })
            .await
            .unwrap_or_else(|e| fail(e));
            let port: u16 = arg(3)
                .parse()
                .unwrap_or_else(|_| fail("port must be a number"));
            let mem: u32 = arg(4)
                .parse()
                .unwrap_or_else(|_| fail("memory must be a number (MiB)"));
            let started = request(
                &mut conn,
                "modpacks.install",
                json!({"project": arg(1), "name": arg(2), "port": port, "memory_mb": mem}),
            )
            .await;
            let task = started["task"].clone();
            loop {
                match conn.recv().await {
                    Ok(Some(Message::Event { data, .. })) if data["task"] == task => {
                        match data["state"].as_str() {
                            Some("completed") => {
                                print(&data["result"]);
                                return;
                            }
                            Some("failed") => fail(&data["error"]),
                            _ => {
                                if let Some(p) = data["phase"].as_str() {
                                    eprintln!("  {p}…");
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => fail("connection lost"),
                }
            }
        }
        "remote-on" => {
            let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(47625);
            let bind = args.get(2).cloned();
            print(
                &request(
                    &mut conn,
                    "remote.enable",
                    json!({"port": port, "bind": bind}),
                )
                .await,
            );
        }
        "remote-off" => print(&request(&mut conn, "remote.disable", Value::Null).await),
        "remote-status" => print(&request(&mut conn, "remote.status", Value::Null).await),
        "pair-code" => {
            let name = args.get(1).cloned();
            let r = request(&mut conn, "pair.create_code", json!({"name": name})).await;
            println!(
                "Pairing code (valid {} minutes, single use):\n\n    {}\n",
                r["expires_in_secs"].as_u64().unwrap_or(600) / 60,
                r["code"].as_str().unwrap_or("")
            );
            println!(
                "This host's fingerprint: {}",
                r["fingerprint"].as_str().unwrap_or("")
            );
        }
        "devices" => print(&request(&mut conn, "devices.list", Value::Null).await),
        "revoke" => print(&request(&mut conn, "devices.revoke", json!({"device": arg(1)})).await),
        "net" => print(&request(&mut conn, "network.check", instance(1)).await),
        "mods" => {
            let r = request(&mut conn, "content.list", instance(1)).await;
            for i in r["items"].as_array().into_iter().flatten() {
                println!(
                    "{:<28} {:<22} {}",
                    i["title"].as_str().unwrap_or(""),
                    i["version_number"].as_str().unwrap_or(""),
                    i["file"].as_str().unwrap_or("")
                );
            }
        }
        "mods-search" => {
            let q = args[2..].join(" ");
            let r = request(
                &mut conn,
                "content.search",
                json!({"instance": arg(1), "query": q}),
            )
            .await;
            for h in r["hits"].as_array().into_iter().flatten() {
                println!(
                    "{:<10} {:<30} {:>10} downloads",
                    h["project_id"].as_str().unwrap_or(""),
                    h["title"].as_str().unwrap_or(""),
                    h["downloads"]
                );
            }
        }
        "mods-plan" => print(
            &request(
                &mut conn,
                "content.plan",
                json!({"instance": arg(1), "project": arg(2)}),
            )
            .await,
        ),
        "mods-rm" => print(
            &request(
                &mut conn,
                "content.remove",
                json!({"instance": arg(1), "project": arg(2)}),
            )
            .await,
        ),
        "mods-add" => {
            conn.send(&Message::Sub {
                topic: "tasks".into(),
                from_seq: None,
            })
            .await
            .unwrap_or_else(|e| fail(e));
            let started = request(
                &mut conn,
                "content.install",
                json!({"instance": arg(1), "project": arg(2)}),
            )
            .await;
            let task = started["task"].clone();
            loop {
                match conn.recv().await {
                    Ok(Some(Message::Event { data, .. })) if data["task"] == task => {
                        match data["state"].as_str() {
                            Some("completed") => {
                                print(&data["result"]);
                                return;
                            }
                            Some("failed") => fail(&data["error"]),
                            _ => {}
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => fail("connection lost"),
                }
            }
        }
        "backups" => {
            let r = request(&mut conn, "backups.list", instance(1)).await;
            for b in r["backups"].as_array().into_iter().flatten() {
                println!(
                    "{}  {:<6} {:>10} bytes  quiesced={}",
                    b["id"].as_str().unwrap_or(""),
                    b["scope"].as_str().unwrap_or(""),
                    b["total_bytes"],
                    b["quiesced"]
                );
            }
        }
        "backup" => {
            conn.send(&Message::Sub {
                topic: "tasks".into(),
                from_seq: None,
            })
            .await
            .unwrap_or_else(|e| fail(e));
            let scope = args.get(2).cloned().unwrap_or_else(|| "full".into());
            let started = request(
                &mut conn,
                "backups.create",
                json!({"instance": arg(1), "scope": scope}),
            )
            .await;
            let task = started["task"].clone();
            loop {
                match conn.recv().await {
                    Ok(Some(Message::Event { data, .. })) if data["task"] == task => {
                        match data["state"].as_str() {
                            Some("completed") => {
                                print(&data["result"]);
                                return;
                            }
                            Some("failed") => fail(&data["error"]),
                            _ => {}
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => fail("connection lost"),
                }
            }
        }
        "verify" => print(
            &request(
                &mut conn,
                "backups.verify",
                json!({"instance": arg(1), "backup": arg(2)}),
            )
            .await,
        ),
        "restore" => print(
            &request(
                &mut conn,
                "backups.restore",
                json!({"instance": arg(1), "backup": arg(2), "confirm": true}),
            )
            .await,
        ),
        "op" | "deop" | "kick" | "ban" | "pardon" | "wl-add" | "wl-remove" => {
            let api = match cmd.as_str() {
                "wl-add" => "players.whitelist_add".to_owned(),
                "wl-remove" => "players.whitelist_remove".to_owned(),
                other => format!("players.{other}"),
            };
            let reason = (args.len() > 3).then(|| args[3..].join(" "));
            let params = json!({"instance": arg(1), "player": arg(2), "reason": reason});
            print(&request(&mut conn, &api, params).await);
        }
        "whitelist" => {
            let enabled = arg(2) == "on";
            let params = json!({"instance": arg(1), "enabled": enabled});
            print(&request(&mut conn, "players.whitelist_set_enabled", params).await);
        }
        "quick" => {
            let cat = request(&mut conn, "commands.catalog", instance(1)).await;
            for a in cat["builtin"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(cat["macros"].as_array().into_iter().flatten())
            {
                let params: Vec<_> = a["params"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["name"].as_str())
                    .collect();
                println!(
                    "{:<18} {}",
                    a["id"].as_str().unwrap_or(""),
                    params.join(" ")
                );
            }
        }
        "run" | "preview" => {
            let mut a = serde_json::Map::new();
            for kv in args.iter().skip(3) {
                let (k, v) = kv
                    .split_once('=')
                    .unwrap_or_else(|| fail("arguments are key=value"));
                let value = match v {
                    "true" => json!(true),
                    "false" => json!(false),
                    n if n.parse::<i64>().is_ok() => json!(n.parse::<i64>().unwrap_or(0)),
                    s => json!(s),
                };
                a.insert(k.to_owned(), value);
            }
            let api = if cmd == "run" {
                "commands.run"
            } else {
                "commands.preview"
            };
            let params = json!({"instance": arg(1), "action": arg(2), "args": a});
            print(&request(&mut conn, api, params).await);
        }
        "send" => {
            let line = args[2..].join(" ");
            print(
                &request(
                    &mut conn,
                    "console.send",
                    json!({"instance": arg(1), "line": line}),
                )
                .await,
            );
        }
        "create" => {
            let port: u16 = arg(3)
                .parse()
                .unwrap_or_else(|_| fail("port must be a number"));
            let mem: u32 = arg(4)
                .parse()
                .unwrap_or_else(|_| fail("memory must be a number (MiB)"));
            conn.send(&Message::Sub {
                topic: "tasks".into(),
                from_seq: None,
            })
            .await
            .unwrap_or_else(|e| fail(e));
            let started = request(
                &mut conn,
                "instances.create",
                json!({"name": arg(1), "mc_version": arg(2), "port": port, "memory_mb": mem, "software": args.get(5).cloned().unwrap_or_else(|| "vanilla".into())}),
            )
            .await;
            let task = started["task"].clone();
            eprintln!("installing (task {task})…");
            loop {
                match conn.recv().await {
                    Ok(Some(Message::Event { data, .. })) if data["task"] == task => {
                        match data["state"].as_str() {
                            Some("completed") => {
                                print(&data["result"]);
                                eprintln!(
                                    "Next: read https://aka.ms/MinecraftEULA, then `msmctl eula <id>`."
                                );
                                return;
                            }
                            Some("failed" | "cancelled") => fail(&data["error"]),
                            _ => {
                                let p = &data["progress"];
                                if let (Some(done), Some(total)) =
                                    (p["done"].as_u64(), p["total"].as_u64())
                                {
                                    eprint!("\r  downloading {}%   ", done * 100 / total.max(1));
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => fail("connection lost"),
                }
            }
        }
        "console" => {
            let id = arg(1);
            let topic = format!("instance/{id}/console");
            conn.send(&Message::Sub {
                topic: topic.clone(),
                from_seq: Some(0),
            })
            .await
            .unwrap_or_else(|e| fail(e));
            let mut stdin = BufReader::new(tokio::io::stdin()).lines();
            let mut stdin_open = true;
            loop {
                tokio::select! {
                    msg = conn.recv() => match msg {
                        Ok(Some(Message::Event { data, .. })) => {
                            if let Some(t) = data["text"].as_str() { println!("{t}"); }
                            if let Some(n) = data["dropped"].as_u64() { println!("[{n} lines skipped]"); }
                        }
                        Ok(Some(Message::Gap { .. })) => println!("[earlier output unavailable]"),
                        Ok(Some(Message::Res { outcome: Outcome::Err(e), .. })) => eprintln!("{}: {}", e.code, e.detail),
                        Ok(Some(_)) => {}
                        _ => break,
                    },
                    line = stdin.next_line(), if stdin_open => match line {
                        Ok(Some(l)) if !l.trim().is_empty() => {
                            let req = Message::Req { id: RequestId::new(), cmd: "console.send".into(), params: json!({"instance": id, "line": l}), idempotency_key: None };
                            if conn.send(&req).await.is_err() { break; }
                        }
                        Ok(Some(_)) => {}
                        // Input closed (e.g. piped): keep streaming output.
                        _ => stdin_open = false,
                    },
                }
            }
        }
        other => fail(format!("unknown command {other}; see `msmctl help`")),
    }
}

fn client_identity(dir: &std::path::Path) -> msm_transport::remote::Identity {
    let path = dir.join("identity.json");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Some(id) = msm_transport::remote::Identity::from_json(&text)
    {
        return id;
    }
    let id = msm_transport::remote::Identity::generate("msmctl").unwrap_or_else(|e| fail(e));
    std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(e));
    std::fs::write(&path, id.to_json()).unwrap_or_else(|e| fail(e));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    id
}

fn hosts(dir: &std::path::Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(dir.join("hosts.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

async fn remote_pair(
    dir: &std::path::Path,
    host: Option<String>,
    code: Option<String>,
    name: Option<String>,
) {
    let (Some(host), Some(code)) = (host, code) else {
        fail("usage: msmctl remote-pair <host:port> <CODE> [name]")
    };
    let id = client_identity(dir);
    let (tls, server_fp) = msm_transport::remote::connect(&host, &id, None)
        .await
        .unwrap_or_else(|e| fail(e));
    let mut conn: Conn = FramedConn::new(Box::new(tls), MAX_CONTROL_FRAME);
    conn.send(&Message::Hello {
        proto: [PROTOCOL_MIN_SUPPORTED, PROTOCOL_MAJOR],
        client: SoftwareInfo {
            name: "msmctl".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume: BTreeMap::new(),
    })
    .await
    .unwrap_or_else(|e| fail(e));
    let _ = conn.recv().await;
    let proof = msm_transport::remote::client_proof(&code, &server_fp, &id.fingerprint());
    let name = name.unwrap_or_else(|| "msmctl".into());
    let r = request(
        &mut conn,
        "pair.complete",
        json!({"proof": proof, "name": name}),
    )
    .await;
    let expected = msm_transport::remote::server_proof(&code, &server_fp, &id.fingerprint());
    if !msm_transport::remote::verify_proof(
        &expected,
        r["server_proof"].as_str().unwrap_or_default(),
    ) {
        fail("the host could not prove it knows the code: do not trust this connection");
    }
    let mut map = hosts(dir);
    map.insert(host.clone(), json!(server_fp));
    std::fs::write(dir.join("hosts.json"), Value::Object(map).to_string())
        .unwrap_or_else(|e| fail(e));
    println!(
        "Paired with {host}. Host fingerprint: {}",
        msm_transport::remote::display_fingerprint(&server_fp)
    );
    println!("Run commands with: msmctl --host {host} <command>");
}

async fn connect_remote(dir: &std::path::Path, host: &str) -> Conn {
    let Some(fp) = hosts(dir)
        .get(host)
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        fail(format!(
            "not paired with {host}; run `msmctl remote-pair {host} <CODE>` first"
        ))
    };
    let id = client_identity(dir);
    let (tls, _) = msm_transport::remote::connect(host, &id, Some(&fp))
        .await
        .unwrap_or_else(|e| fail(e));
    let mut conn: Conn = FramedConn::new(Box::new(tls), MAX_CONTROL_FRAME);
    conn.send(&Message::Hello {
        proto: [PROTOCOL_MIN_SUPPORTED, PROTOCOL_MAJOR],
        client: SoftwareInfo {
            name: "msmctl".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume: BTreeMap::new(),
    })
    .await
    .unwrap_or_else(|e| fail(e));
    match conn.recv().await {
        Ok(Some(Message::HelloOk { .. })) => conn,
        other => fail(format!("unexpected reply: {other:?}")),
    }
}
