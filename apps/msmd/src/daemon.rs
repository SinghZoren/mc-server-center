//! Daemon state and command handlers.
//!
//! The dispatcher looks every command up in the registry, checks the caller's capability, then
//! calls a handler. Instance lifecycle changes always go through the pure state machine in
//! `msm-core`, are persisted, published on `instance/<id>/state`, and audited.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use msm_api::commands::lookup;
use msm_api::{ApiError, Principal};
use msm_core::ids::{InstanceId, TaskId};
use msm_core::instance_state::{InstanceEvent, InstanceState, Transition, transition};
use msm_core::mc::command::{ArgValue, PlayerName};
use msm_core::{ErrorCode, MsmError};
use msm_engine::backups;
use msm_engine::content;
use msm_engine::java;
use msm_engine::players::{self, Moderation, OnlineTracker};
use msm_engine::process::LaunchSpec;

use crate::runner::RunnerSpec;
use crate::runner_client::RunnerClient;
use msm_engine::quick_commands::{self, ActionDef};
use msm_engine::vanilla::{self, InstallDirs, InstanceManifest, VanillaRequest};
use msm_providers::http::Downloader;
use msm_providers::mojang::MojangClient;
use msm_storage::db::now_ms;
use msm_storage::layout::DataDir;
use msm_storage::repo::{AuditEntry, AuditOutcome, append_audit};
use msm_storage::rusqlite::{Connection, params};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

use crate::events::EventHub;

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(600);

/// One managed server.
#[derive(Debug)]
pub struct Instance {
    pub manifest: InstanceManifest,
    pub dir: PathBuf,
    state: Mutex<InstanceState>,
    process: Mutex<Option<Arc<RunnerClient>>>,
    /// Serializes lifecycle commands per instance.
    op: tokio::sync::Mutex<()>,
    /// Players currently online (from console lines).
    online: Mutex<OnlineTracker>,
    /// Findings from the last unexpected exit.
    findings: Mutex<Vec<msm_engine::diagnostics::Finding>>,
}

impl Instance {
    fn state(&self) -> InstanceState {
        self.state
            .lock()
            .map(|s| *s)
            .unwrap_or(InstanceState::Unknown)
    }
    fn process(&self) -> Option<Arc<RunnerClient>> {
        self.process.lock().ok().and_then(|p| p.clone())
    }
    fn online_players(&self) -> Vec<players::OnlinePlayer> {
        self.online.lock().map(|t| t.online()).unwrap_or_default()
    }
    fn server_dir(&self) -> PathBuf {
        self.dir.join("server")
    }
    fn summary(&self) -> Value {
        json!({
            "id": self.manifest.id,
            "name": self.manifest.name,
            "software": self.manifest.software,
            "build": self.manifest.build,
            "mc_version": self.manifest.mc_version,
            "java_major": self.manifest.java_major,
            "port": self.manifest.port,
            "memory_mb": self.manifest.memory_mb,
            "state": self.state(),
            "eula_accepted": vanilla::eula_accepted(&self.dir),
            "pid": self.process().and_then(|p| p.pid()),
        })
    }
}

/// Shared daemon state.
#[derive(Debug)]
pub struct Daemon {
    pub data: DataDir,
    db: Mutex<Connection>,
    downloader: Downloader,
    mojang: MojangClient,
    instances: tokio::sync::RwLock<HashMap<InstanceId, Arc<Instance>>>,
    pub events: EventHub,
    tasks: Mutex<HashMap<TaskId, (CancellationToken, Value)>>,
    remote: crate::remote_server::RemoteState,
    /// Signalled by `system.shutdown`.
    shutdown: tokio::sync::Notify,
    relay_hub: crate::relay::RelayHub,
    relay_client: crate::relay::RelayClient,
}

type Res = Result<Value, ApiError>;

fn err(code: ErrorCode, detail: impl Into<String>) -> ApiError {
    ApiError::new(code, detail)
}

fn params<T: DeserializeOwned>(v: &Value) -> Result<T, ApiError> {
    serde_json::from_value(v.clone()).map_err(|e| err(ErrorCode::InvalidArgument, e.to_string()))
}

#[derive(Deserialize)]
struct InstanceRef {
    instance: InstanceId,
}

impl Daemon {
    /// Resolves when `system.shutdown` was requested.
    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }

    /// Opens state and loads existing instances.
    ///
    /// # Errors
    /// Database or filesystem errors.
    pub async fn open(data: DataDir) -> Result<Arc<Self>, MsmError> {
        let conn = msm_storage::db::open(&data.db_path(), &data.db_backups())?;
        let downloader = Downloader::new(data.blob_cache(), data.tmp_downloads());
        let mojang = MojangClient::new(downloader.clone());
        let daemon = Arc::new(Self {
            data,
            db: Mutex::new(conn),
            downloader,
            mojang,
            instances: tokio::sync::RwLock::new(HashMap::new()),
            events: EventHub::default(),
            tasks: Mutex::new(HashMap::new()),
            remote: crate::remote_server::RemoteState::default(),
            shutdown: tokio::sync::Notify::new(),
            relay_hub: crate::relay::RelayHub::default(),
            relay_client: crate::relay::RelayClient::default(),
        });
        daemon.load_instances().await?;
        Ok(daemon)
    }

    async fn load_instances(self: &Arc<Self>) -> Result<(), MsmError> {
        let rows: Vec<(String, String, String)> = {
            let db = self
                .db
                .lock()
                .map_err(|_| MsmError::new(ErrorCode::Internal, "db lock"))?;
            let mut stmt = db
                .prepare("SELECT id, install_root, state FROM instances WHERE deleted_at IS NULL")
                .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .and_then(Iterator::collect)
                .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?
        };
        let mut map = self.instances.write().await;
        for (id, root, state) in rows {
            let dir = PathBuf::from(root);
            let Ok(manifest) = InstanceManifest::load(&dir) else {
                continue;
            };
            let previous: InstanceState =
                serde_json::from_value(json!(state)).unwrap_or(InstanceState::Unknown);
            // Reconcile: a server that was live is re-attached through its runner, verified by
            // the nonce in runner.json. If a runner file exists but nothing trustworthy answers,
            // the state is Unknown and the process is never touched. No runner file means the
            // runner exited (and with it the server).
            let mut attached = None;
            let info_path = dir.join("runner.json");
            let now = if previous.has_live_process() || previous == InstanceState::Reconciling {
                match crate::runner_client::read_info(&info_path) {
                    Some(info) => match RunnerClient::attach(&info).await {
                        Ok(client) => {
                            let s = if client.is_ready() {
                                InstanceState::Running
                            } else {
                                InstanceState::Starting
                            };
                            attached = Some(client);
                            s
                        }
                        Err(_) => InstanceState::Unknown,
                    },
                    None => InstanceState::Stopped,
                }
            } else if previous == InstanceState::Installing {
                InstanceState::InstallFailed
            } else {
                previous
            };
            let Ok(parsed) = id.parse::<InstanceId>() else {
                continue;
            };
            let inst = Arc::new(Instance {
                manifest,
                dir,
                state: Mutex::new(now),
                process: Mutex::new(attached.clone()),
                op: tokio::sync::Mutex::new(()),
                online: Mutex::new(OnlineTracker::default()),
                findings: Mutex::new(Vec::new()),
            });
            map.insert(parsed, inst.clone());
            self.persist_state(parsed, now);
            if let Some(client) = attached {
                self.spawn_monitor(inst, client);
            }
        }
        Ok(())
    }

    fn persist_state(&self, id: InstanceId, state: InstanceState) {
        if let Ok(db) = self.db.lock() {
            let s = serde_json::to_value(state)
                .ok()
                .and_then(|v| v.as_str().map(String::from));
            let _ = db.execute(
                "UPDATE instances SET state = ?1, state_version = state_version + 1, updated_at = ?2 WHERE id = ?3",
                params![s, now_ms(), id.to_string()],
            );
        }
    }

    fn audit(
        &self,
        principal: &Principal,
        action: &str,
        target: Option<String>,
        outcome: Result<(), ErrorCode>,
    ) {
        if let Ok(db) = self.db.lock() {
            let label = principal.audit_label();
            let _ = append_audit(
                &db,
                &AuditEntry {
                    principal: &label,
                    action,
                    target: target.as_deref(),
                    params: None,
                    outcome: if outcome.is_ok() {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Error
                    },
                    error_code: outcome.err(),
                },
            );
        }
    }

    /// Applies a lifecycle event through the state machine; persists and publishes changes.
    fn apply(&self, inst: &Instance, event: InstanceEvent) -> Result<Transition, ApiError> {
        let mut guard = inst
            .state
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "state lock"))?;
        let t = transition(*guard, event).map_err(|e| err(e.code(), e.to_string()))?;
        if let Transition::Changed { to, .. } = &t {
            *guard = *to;
            drop(guard);
            self.persist_state(inst.manifest.id, *to);
            self.events.publish(
                &format!("instance/{}/state", inst.manifest.id),
                json!({ "state": to }),
            );
        }
        Ok(t)
    }

    async fn instance(&self, id: InstanceId) -> Result<Arc<Instance>, ApiError> {
        self.instances
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| err(ErrorCode::NotFound, format!("instance {id}")))
    }

    /// Entry point for every request.
    pub async fn dispatch(self: &Arc<Self>, principal: &Principal, cmd: &str, p: &Value) -> Res {
        let Some(spec) = lookup(cmd) else {
            return Err(err(
                ErrorCode::InvalidArgument,
                format!("unknown command {cmd}"),
            ));
        };
        if !principal.has(spec.capability) {
            self.audit(principal, cmd, None, Err(ErrorCode::Forbidden));
            return Err(err(
                ErrorCode::Forbidden,
                format!("{cmd} requires {:?}", spec.capability),
            ));
        }
        let result = self.handle(principal, cmd, p).await;
        if spec.mutating {
            let target = p
                .get("instance")
                .and_then(Value::as_str)
                .map(|s| format!("instance:{s}"));
            self.audit(
                principal,
                cmd,
                target,
                result.as_ref().map(|_| ()).map_err(|e| e.code),
            );
        }
        result
    }

    async fn handle(self: &Arc<Self>, principal: &Principal, cmd: &str, p: &Value) -> Res {
        match cmd {
            "system.ping" => Ok(json!("pong")),
            "system.info" => Ok(json!({
                "daemon": "msmd",
                "version": env!("CARGO_PKG_VERSION"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "data_dir": self.data.root(),
                // Lets the desktop app tell whether this is the service it shipped with.
                "exe": std::env::current_exe().ok(),
                "pid": std::process::id(),
            })),
            "catalog.list" => {
                #[derive(Deserialize, Default)]
                struct P {
                    #[serde(default)]
                    software: vanilla::Software,
                }
                let P { software } = if p.is_null() {
                    P::default()
                } else {
                    params(p)?
                };
                self.catalog(software).await
            }
            "network.info" => {
                let lan = msm_engine::network::primary_lan_ipv4();
                Ok(json!({
                    "lan_ipv4": lan,
                    "lan_kind": lan.map(msm_engine::network::classify),
                }))
            }
            "java.install" => {
                #[derive(Deserialize)]
                struct P {
                    major: u32,
                }
                let P { major } = params(p)?;
                if !(8..=99).contains(&major) {
                    return Err(err(ErrorCode::InvalidArgument, "unsupported Java version"));
                }
                let task = TaskId::new();
                let cancel = CancellationToken::new();
                let record = json!({"task": task, "kind": "java.install", "state": "active", "phase": "java"});
                if let Ok(mut t) = self.tasks.lock() {
                    t.insert(task, (cancel.clone(), record.clone()));
                }
                self.events.publish("tasks", record);
                let me = self.clone();
                tokio::spawn(async move {
                    let record = match me.ensure_java(task, major, &cancel).await {
                        Ok(rt) => {
                            json!({"task": task, "kind": "java.install", "state": "completed", "result": rt})
                        }
                        Err(e) => {
                            json!({"task": task, "kind": "java.install", "state": "failed", "error": e})
                        }
                    };
                    if let Ok(mut t) = me.tasks.lock()
                        && let Some(entry) = t.get_mut(&task)
                    {
                        entry.1 = record.clone();
                    }
                    me.events.publish("tasks", record);
                });
                Ok(json!({"task": task}))
            }
            "java.list" => {
                let found = java::discover(&[self.data.java_runtimes()]).await;
                Ok(serde_json::to_value(found).unwrap_or_default())
            }
            "tasks.list" => Ok(Value::Array(
                self.tasks
                    .lock()
                    .map(|t| t.values().map(|(_, v)| v.clone()).collect())
                    .unwrap_or_default(),
            )),
            "tasks.cancel" => {
                #[derive(Deserialize)]
                struct P {
                    task: TaskId,
                }
                let P { task } = params(p)?;
                let token = self
                    .tasks
                    .lock()
                    .ok()
                    .and_then(|t| t.get(&task).map(|(c, _)| c.clone()));
                token
                    .ok_or_else(|| err(ErrorCode::NotFound, "task"))?
                    .cancel();
                Ok(json!({"cancelled": task}))
            }
            "instances.list" => {
                let map = self.instances.read().await;
                Ok(Value::Array(map.values().map(|i| i.summary()).collect()))
            }
            "instances.get" => {
                let InstanceRef { instance } = params(p)?;
                Ok(self.instance(instance).await?.summary())
            }
            "instances.create" => self.create(params(p)?).await,
            "instances.accept_eula" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    accept: bool,
                }
                let P { instance, accept } = params(p)?;
                if !accept {
                    return Err(err(ErrorCode::InvalidArgument, "accept must be true"));
                }
                let inst = self.instance(instance).await?;
                vanilla::accept_eula(&inst.dir).map_err(ApiError::from)?;
                if let Ok(db) = self.db.lock() {
                    let _ = db.execute(
                        "UPDATE instances SET eula_accepted_at = ?1, eula_accepted_by = ?2 WHERE id = ?3",
                        params![now_ms(), principal.audit_label(), instance.to_string()],
                    );
                }
                Ok(inst.summary())
            }
            "instances.start" => {
                let InstanceRef { instance } = params(p)?;
                self.start(self.instance(instance).await?).await
            }
            "instances.stop" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    timeout_secs: Option<u64>,
                }
                let P {
                    instance,
                    timeout_secs,
                } = params(p)?;
                let timeout = timeout_secs.map_or(DEFAULT_STOP_TIMEOUT, Duration::from_secs);
                self.stop(self.instance(instance).await?, timeout).await
            }
            "instances.kill" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    confirm: bool,
                }
                let P { instance, confirm } = params(p)?;
                if !confirm {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "force kill can lose recent world changes; resend with confirm=true",
                    ));
                }
                self.kill(self.instance(instance).await?).await
            }
            "console.send" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    line: String,
                }
                let P { instance, line } = params(p)?;
                let inst = self.instance(instance).await?;
                let proc = inst
                    .process()
                    .ok_or_else(|| err(ErrorCode::ConsoleUnavailable, "server is not running"))?;
                let line = line.trim().trim_start_matches('/');
                proc.send_line(line)
                    .await
                    .map_err(|e| err(ErrorCode::InvalidArgument, e.to_string()))?;
                Ok(json!({"sent": true}))
            }
            "players.list" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                Ok(json!({
                    "running": inst.process().is_some(),
                    "online": inst.online_players(),
                    "lists": players::read_lists(&inst.server_dir()),
                }))
            }
            "players.op"
            | "players.deop"
            | "players.kick"
            | "players.ban"
            | "players.pardon"
            | "players.whitelist_add"
            | "players.whitelist_remove" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    player: String,
                    #[serde(default)]
                    reason: Option<String>,
                }
                let P {
                    instance,
                    player,
                    reason,
                } = params(p)?;
                let action = match cmd {
                    "players.op" => Moderation::Op,
                    "players.deop" => Moderation::Deop,
                    "players.kick" => Moderation::Kick { reason },
                    "players.ban" => Moderation::Ban { reason },
                    "players.pardon" => Moderation::Pardon,
                    "players.whitelist_add" => Moderation::WhitelistAdd,
                    _ => Moderation::WhitelistRemove,
                };
                self.moderate(self.instance(instance).await?, &action, &player)
                    .await
            }
            "players.whitelist_set_enabled" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    enabled: bool,
                }
                let P { instance, enabled } = params(p)?;
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                if let Some(proc) = inst.process() {
                    let line = if enabled {
                        "whitelist on"
                    } else {
                        "whitelist off"
                    };
                    proc.send_line(line)
                        .await
                        .map_err(|e| err(ErrorCode::ConsoleUnavailable, e.to_string()))?;
                    Ok(json!({"applied": "console", "command": line}))
                } else {
                    let path = inst.server_dir().join("server.properties");
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let mut doc = msm_core::mc::properties::PropertiesDoc::parse(&text);
                    doc.set("white-list", if enabled { "true" } else { "false" });
                    msm_fsguard::write_atomic(&path, doc.render().as_bytes())
                        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                    Ok(json!({"applied": "file"}))
                }
            }
            "commands.catalog" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                Ok(json!({
                    "builtin": quick_commands::builtin_actions(&inst.manifest.mc_version),
                    "macros": self.macros_for(Some(instance))?,
                }))
            }
            "commands.preview" | "commands.run" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    action: String,
                    #[serde(default)]
                    args: BTreeMap<String, ArgValue>,
                }
                let P {
                    instance,
                    action,
                    args,
                } = params(p)?;
                let inst = self.instance(instance).await?;
                let def = self.find_action(&inst, &action)?;
                let lines = def
                    .render(&args)
                    .map_err(|e| err(ErrorCode::InvalidArgument, e.to_string()))?;
                if cmd == "commands.preview" {
                    return Ok(json!({"lines": lines}));
                }
                let proc = inst
                    .process()
                    .ok_or_else(|| err(ErrorCode::ConsoleUnavailable, "server is not running"))?;
                for line in &lines {
                    proc.send_line(line)
                        .await
                        .map_err(|e| err(ErrorCode::ConsoleUnavailable, e.to_string()))?;
                }
                Ok(json!({"sent": lines}))
            }
            "macros.list" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    instance: Option<InstanceId>,
                }
                let P { instance } = params(p)?;
                Ok(json!(self.macros_for(instance)?))
            }
            "macros.save" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    instance: Option<InstanceId>,
                    #[serde(rename = "macro")]
                    def: ActionDef,
                }
                let P { instance, mut def } = params(p)?;
                if def.id.parse::<msm_core::ids::RequestId>().is_err() {
                    def.id = msm_core::ids::RequestId::new().to_string();
                }
                def.category = "macro".into();
                quick_commands::validate_macro(&def)
                    .map_err(|e| err(ErrorCode::InvalidArgument, e.to_string()))?;
                let db = self
                    .db
                    .lock()
                    .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
                db.execute(
                    "INSERT INTO command_macros (id, instance_id, name, def_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                     ON CONFLICT (id) DO UPDATE SET name = excluded.name, def_json = excluded.def_json, updated_at = excluded.updated_at",
                    params![
                        def.id,
                        instance.map(|i| i.to_string()),
                        def.label,
                        serde_json::to_string(&def).unwrap_or_default(),
                        now_ms()
                    ],
                )
                .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                Ok(json!(def))
            }
            "macros.delete" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let P { id } = params(p)?;
                let db = self
                    .db
                    .lock()
                    .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
                let n = db
                    .execute("DELETE FROM command_macros WHERE id = ?1", params![id])
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                if n == 0 {
                    return Err(err(ErrorCode::NotFound, "macro"));
                }
                Ok(json!({"deleted": id}))
            }
            "backups.list" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let dir = inst.dir.join("backups");
                let list = tokio::task::spawn_blocking(move || backups::list(&dir))
                    .await
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                let estimate_dir = inst.server_dir();
                let full = tokio::task::spawn_blocking(move || {
                    backups::estimate(&estimate_dir, backups::Scope::Full).ok()
                })
                .await
                .ok()
                .flatten();
                Ok(json!({"backups": list, "estimate_full_bytes": full}))
            }
            "backups.create" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    #[serde(default = "full_scope")]
                    scope: backups::Scope,
                }
                fn full_scope() -> backups::Scope {
                    backups::Scope::Full
                }
                let P { instance, scope } = params(p)?;
                let inst = self.instance(instance).await?;
                let task = TaskId::new();
                let record = json!({"task": task, "kind": "backups.create", "state": "active", "instance": instance});
                if let Ok(mut t) = self.tasks.lock() {
                    t.insert(task, (CancellationToken::new(), record.clone()));
                }
                self.events.publish("tasks", record);
                let me = self.clone();
                tokio::spawn(async move {
                    let result = me.backup_now(&inst, scope, "manual").await;
                    let record = match result {
                        Ok(m) => {
                            json!({"task": task, "kind": "backups.create", "state": "completed", "instance": instance, "result": {"id": m.id, "files": m.files.len(), "bytes": m.total_bytes, "quiesced": m.quiesced}})
                        }
                        Err(e) => {
                            json!({"task": task, "kind": "backups.create", "state": "failed", "instance": instance, "error": e})
                        }
                    };
                    if let Ok(mut t) = me.tasks.lock()
                        && let Some(entry) = t.get_mut(&task)
                    {
                        entry.1 = record.clone();
                    }
                    me.events.publish("tasks", record);
                });
                Ok(json!({"task": task}))
            }
            "backups.verify" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    backup: String,
                }
                let P { instance, backup } = params(p)?;
                let inst = self.instance(instance).await?;
                let zip = backup_path(&inst, &backup)?;
                let (_, v) = tokio::task::spawn_blocking(move || backups::verify(&zip))
                    .await
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
                    .map_err(ApiError::from)?;
                Ok(json!(v))
            }
            "backups.restore" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    backup: String,
                    confirm: bool,
                }
                let P {
                    instance,
                    backup,
                    confirm,
                } = params(p)?;
                if !confirm {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "restore replaces the current files; resend with confirm=true",
                    ));
                }
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                if !matches!(
                    inst.state(),
                    InstanceState::Stopped | InstanceState::Crashed
                ) {
                    return Err(err(
                        ErrorCode::InvalidStateTransition,
                        "stop the server before restoring a backup",
                    ));
                }
                let zip = backup_path(&inst, &backup)?;
                let (dir, stage) = (inst.dir.clone(), self.data.tmp_stage());
                let kept =
                    tokio::task::spawn_blocking(move || backups::restore(&zip, &dir, &stage))
                        .await
                        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
                        .map_err(ApiError::from)?;
                Ok(json!({"restored": backup, "previous_kept_at": kept}))
            }
            "backups.delete" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    backup: String,
                }
                let P { instance, backup } = params(p)?;
                let inst = self.instance(instance).await?;
                let zip = backup_path(&inst, &backup)?;
                std::fs::remove_file(&zip).map_err(|e| err(ErrorCode::NotFound, e.to_string()))?;
                let _ = std::fs::remove_file(zip.with_extension("json"));
                Ok(json!({"deleted": backup}))
            }
            "config.get" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                Ok(json!({
                    "schema": msm_engine::settings::schema(),
                    "values": msm_engine::settings::read(&inst.server_dir()),
                    "running": inst.process().is_some(),
                }))
            }
            "config.set_typed" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    values: BTreeMap<String, String>,
                }
                let P { instance, values } = params(p)?;
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                let changed = msm_engine::settings::write(
                    &inst.server_dir(),
                    &inst.dir.join("config-history"),
                    &values,
                    now_ms(),
                )
                .map_err(ApiError::from)?;
                let running = inst.process().is_some();
                Ok(json!({"changed": changed, "restart_required": running && !changed.is_empty()}))
            }
            "diagnostics.list" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let findings = inst.findings.lock().map(|f| f.clone()).unwrap_or_default();
                Ok(json!({"findings": findings}))
            }
            "instances.update" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    #[serde(default)]
                    name: Option<String>,
                    #[serde(default)]
                    memory_mb: Option<u32>,
                }
                let P {
                    instance,
                    name,
                    memory_mb,
                } = params(p)?;
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                // The instance record is replaced, so no process or monitor may be attached.
                if inst.process().is_some()
                    || !matches!(
                        inst.state(),
                        InstanceState::Stopped
                            | InstanceState::Crashed
                            | InstanceState::InstallFailed
                    )
                {
                    return Err(err(
                        ErrorCode::InvalidStateTransition,
                        "stop the server before changing it",
                    ));
                }
                let mut manifest = inst.manifest.clone();
                if let Some(n) = name {
                    let n = n.trim().to_owned();
                    if n.is_empty() || n.chars().count() > 64 || n.chars().any(char::is_control) {
                        return Err(err(
                            ErrorCode::InvalidArgument,
                            "name must be 1-64 printable characters",
                        ));
                    }
                    manifest.name = n;
                }
                if let Some(m) = memory_mb {
                    if !(512..=262_144).contains(&m) {
                        return Err(err(
                            ErrorCode::InvalidArgument,
                            "memory must be between 512 MiB and 256 GiB",
                        ));
                    }
                    manifest.memory_mb = m;
                }
                let text = toml::to_string_pretty(&manifest)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                msm_fsguard::write_atomic(&inst.dir.join("instance.toml"), text.as_bytes())
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                if let Ok(db) = self.db.lock() {
                    let _ = db.execute(
                        "UPDATE instances SET name = ?1, memory_json = ?2, updated_at = ?3 WHERE id = ?4",
                        params![manifest.name, json!({"max_mb": manifest.memory_mb}).to_string(), now_ms(), instance.to_string()],
                    );
                }
                let updated = Arc::new(Instance {
                    manifest,
                    dir: inst.dir.clone(),
                    state: Mutex::new(inst.state()),
                    process: Mutex::new(inst.process()),
                    op: tokio::sync::Mutex::new(()),
                    online: Mutex::new(inst.online.lock().map(|t| t.clone()).unwrap_or_default()),
                    findings: Mutex::new(
                        inst.findings.lock().map(|f| f.clone()).unwrap_or_default(),
                    ),
                });
                let summary = updated.summary();
                self.instances.write().await.insert(instance, updated);
                self.events.publish(
                    &format!("instance/{instance}/state"),
                    json!({"state": inst.state()}),
                );
                Ok(summary)
            }
            "instances.delete" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    confirm: bool,
                }
                let P { instance, confirm } = params(p)?;
                if !confirm {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "deleting a server needs confirm=true",
                    ));
                }
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                if !matches!(
                    inst.state(),
                    InstanceState::Stopped | InstanceState::Crashed | InstanceState::InstallFailed
                ) {
                    return Err(err(
                        ErrorCode::InvalidStateTransition,
                        "stop the server before deleting it",
                    ));
                }
                // Recoverable: move the whole folder to trash/ instead of deleting it.
                let trash = self.data.root().join("trash");
                std::fs::create_dir_all(&trash)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                let dest = trash.join(format!("{}-{}", instance, now_ms()));
                msm_fsguard::rename_with_retry(&inst.dir, &dest).map_err(|e| {
                    err(
                        ErrorCode::Internal,
                        format!("could not move the server folder (is a file open in it?): {e}"),
                    )
                })?;
                if let Ok(db) = self.db.lock() {
                    let _ = db.execute(
                        "UPDATE instances SET deleted_at = ?1, install_root = ?2 WHERE id = ?3",
                        params![now_ms(), dest.to_string_lossy(), instance.to_string()],
                    );
                }
                self.instances.write().await.remove(&instance);
                self.events.publish(
                    &format!("instance/{instance}/state"),
                    json!({"state": "deleted"}),
                );
                Ok(json!({"deleted": instance, "moved_to": dest}))
            }
            "content.search" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    #[serde(default)]
                    query: String,
                    #[serde(default)]
                    offset: u32,
                }
                let P {
                    instance,
                    query,
                    offset,
                } = params(p)?;
                let inst = self.instance(instance).await?;
                let (kind, _) = content_kind(&inst)?;
                let r = msm_providers::modrinth::search(
                    &self.downloader,
                    kind,
                    &inst.manifest.mc_version,
                    &query,
                    offset,
                )
                .await
                .map_err(|e| ApiError::from(MsmError::from(e)))?;
                Ok(json!(r))
            }
            "content.plan" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    project: String,
                }
                let P { instance, project } = params(p)?;
                let inst = self.instance(instance).await?;
                let (kind, _) = content_kind(&inst)?;
                let src = ModrinthSource {
                    dl: self.downloader.clone(),
                    kind,
                    mc: inst.manifest.mc_version.clone(),
                };
                let plan = msm_engine::content::plan(
                    &src,
                    &project,
                    &content::Registry::load(&inst.server_dir()),
                )
                .await
                .map_err(ApiError::from)?;
                Ok(json!(plan))
            }
            "content.install" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    project: String,
                }
                let P { instance, project } = params(p)?;
                let inst = self.instance(instance).await?;
                let (kind, target) = content_kind(&inst)?;
                let task = TaskId::new();
                let cancel = CancellationToken::new();
                let record = json!({"task": task, "kind": "content.install", "state": "active", "instance": instance});
                if let Ok(mut t) = self.tasks.lock() {
                    t.insert(task, (cancel.clone(), record.clone()));
                }
                self.events.publish("tasks", record);
                let me = self.clone();
                tokio::spawn(async move {
                    let result = me
                        .install_content(&inst, kind, target, &project, &cancel)
                        .await;
                    let record = match result {
                        Ok(v) => {
                            json!({"task": task, "kind": "content.install", "state": "completed", "instance": instance, "result": v})
                        }
                        Err(e) => {
                            json!({"task": task, "kind": "content.install", "state": "failed", "instance": instance, "error": e})
                        }
                    };
                    if let Ok(mut t) = me.tasks.lock()
                        && let Some(entry) = t.get_mut(&task)
                    {
                        entry.1 = record.clone();
                    }
                    me.events.publish("tasks", record);
                });
                Ok(json!({"task": task}))
            }
            "content.list" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let supported = content_kind(&inst).is_ok();
                Ok(
                    json!({"supported": supported, "items": content::Registry::load(&inst.server_dir()).items}),
                )
            }
            "content.remove" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    project: String,
                }
                let P { instance, project } = params(p)?;
                let inst = self.instance(instance).await?;
                let _op = inst.op.lock().await;
                self.backup_now(
                    &inst,
                    backups::Scope::Content,
                    "safety: before removing a mod",
                )
                .await?;
                let server = inst.server_dir();
                let dependents = content::remove(&server, &project).map_err(ApiError::from)?;
                Ok(
                    json!({"removed": project, "may_break": dependents, "restart_required": inst.process().is_some()}),
                )
            }
            "modpacks.search" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    query: String,
                }
                let P { query } = params(p)?;
                let r = msm_providers::modrinth::search_modpacks(&self.downloader, &query, 0)
                    .await
                    .map_err(|e| ApiError::from(MsmError::from(e)))?;
                Ok(json!(r))
            }
            "modpacks.install" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    name: String,
                    port: u16,
                    memory_mb: u32,
                }
                let req: P = params(p)?;
                self.port_available(req.port).await?;
                let task = TaskId::new();
                let cancel = CancellationToken::new();
                let record = json!({"task": task, "kind": "modpacks.install", "state": "active", "phase": "pack"});
                if let Ok(mut t) = self.tasks.lock() {
                    t.insert(task, (cancel.clone(), record.clone()));
                }
                self.events.publish("tasks", record);
                let me = self.clone();
                tokio::spawn(async move {
                    let result = me
                        .install_modpack(
                            task,
                            &req.project,
                            req.name,
                            req.port,
                            req.memory_mb,
                            &cancel,
                        )
                        .await;
                    let record = match result {
                        Ok(v) => {
                            json!({"task": task, "kind": "modpacks.install", "state": "completed", "result": v})
                        }
                        Err(e) => {
                            json!({"task": task, "kind": "modpacks.install", "state": "failed", "error": e})
                        }
                    };
                    if let Ok(mut t) = me.tasks.lock()
                        && let Some(entry) = t.get_mut(&task)
                    {
                        entry.1 = record.clone();
                    }
                    me.events.publish("tasks", record);
                });
                Ok(json!({"task": task}))
            }
            "network.check" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let lan = msm_engine::network::primary_lan_ipv4();
                let (public, wan) = tokio::join!(
                    msm_engine::network::public_ipv4(&self.downloader),
                    msm_engine::network::router_wan_ip()
                );
                let verdict = msm_engine::network::assess(public.ip, wan);
                let upnp_mapped = inst.dir.join("upnp.json").is_file();
                Ok(json!({
                    "port": inst.manifest.port,
                    "lan_ipv4": lan,
                    "public": public,
                    "router_wan_ip": wan,
                    "upnp_available": wan.is_some(),
                    "upnp_mapped": upnp_mapped,
                    "verdict": verdict,
                    "firewall_rule": inst.dir.join("firewall.json").is_file(),
                }))
            }
            "network.upnp_open" | "network.upnp_close" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let port = inst.manifest.port;
                let record = inst.dir.join("upnp.json");
                if cmd == "network.upnp_open" {
                    let lan = msm_engine::network::primary_lan_ipv4().ok_or_else(|| {
                        err(
                            ErrorCode::UpstreamError,
                            "this PC's network address could not be found",
                        )
                    })?;
                    msm_engine::network::upnp_open(port, lan)
                        .await
                        .map_err(|e| err(ErrorCode::UpstreamError, e))?;
                    msm_fsguard::write_atomic(
                        &record,
                        json!({"port": port, "lan_ip": lan, "at": now_ms()})
                            .to_string()
                            .as_bytes(),
                    )
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                    Ok(json!({"opened": port}))
                } else {
                    msm_engine::network::upnp_close(port)
                        .await
                        .map_err(|e| err(ErrorCode::UpstreamError, e))?;
                    let _ = std::fs::remove_file(&record);
                    Ok(json!({"closed": port}))
                }
            }
            "network.external_test" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    consent: bool,
                }
                let P { instance, consent } = params(p)?;
                if !consent {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "the outside check sends your public address to mcsrvstat.us; resend with consent=true",
                    ));
                }
                let inst = self.instance(instance).await?;
                let public = msm_engine::network::public_ipv4(&self.downloader).await;
                let Some(ip) = public.ip else {
                    return Ok(
                        json!({"status": "not_tested", "source": "none", "note": "Your public address could not be determined."}),
                    );
                };
                Ok(json!(
                    msm_engine::network::external_check(&self.downloader, ip, inst.manifest.port)
                        .await
                ))
            }
            "network.firewall_allow" | "network.firewall_remove" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let allow = cmd == "network.firewall_allow";
                firewall_rule(&inst, allow).await?;
                let record = inst.dir.join("firewall.json");
                if allow {
                    let _ = msm_fsguard::write_atomic(
                        &record,
                        json!({"port": inst.manifest.port}).to_string().as_bytes(),
                    );
                } else {
                    let _ = std::fs::remove_file(&record);
                }
                Ok(json!({"ok": true}))
            }
            "system.stop_all" => {
                let all: Vec<_> = self.instances.read().await.values().cloned().collect();
                let mut stopping = Vec::new();
                for inst in all {
                    if inst.process().is_some() {
                        let _ = self.stop(inst.clone(), DEFAULT_STOP_TIMEOUT).await;
                        stopping.push(inst.manifest.id);
                    }
                }
                Ok(json!({"stopping": stopping}))
            }
            "system.shutdown" => {
                // By default stops only this service (used by the desktop app after an update so
                // the new version takes over): servers keep running in their runners and are
                // re-attached by the next service. With `stop_servers`, servers are first stopped
                // cleanly (worlds saved), force-killed only if they don't stop in time; used when
                // the user quits the app.
                if p.get("stop_servers").and_then(Value::as_bool) == Some(true) {
                    let all: Vec<_> = self.instances.read().await.values().cloned().collect();
                    let mut stops = tokio::task::JoinSet::new();
                    for inst in all {
                        let Some(proc) = inst.process() else { continue };
                        let me = self.clone();
                        stops.spawn(async move {
                            let _ = me.stop(inst.clone(), DEFAULT_STOP_TIMEOUT).await;
                            if tokio::time::timeout(Duration::from_secs(50), proc.wait_exit()).await.is_err() {
                                let _ = me.kill(inst).await;
                            }
                        });
                    }
                    while stops.join_next().await.is_some() {}
                }
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    me.shutdown.notify_one();
                });
                Ok(json!({"shutting_down": true}))
            }
            "system.running_count" => {
                let n = self
                    .instances
                    .read()
                    .await
                    .values()
                    .filter(|i| i.process().is_some())
                    .count();
                Ok(json!({"running": n}))
            }
            "remote.status" => {
                let port = self.remote_port_setting();
                let fp = crate::remote_server::load_identity(self)
                    .map(|i| i.fingerprint())
                    .ok();
                Ok(json!({
                    "enabled": port.is_some(),
                    "listening": self.remote.is_listening(),
                    "port": port,
                    "fingerprint": fp.as_deref().map(msm_transport::remote::display_fingerprint),
                }))
            }
            "remote.enable" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default = "default_remote_port")]
                    port: u16,
                    /// Address to listen on; all interfaces by default.
                    #[serde(default)]
                    bind: Option<std::net::IpAddr>,
                }
                fn default_remote_port() -> u16 {
                    47625
                }
                let P { port, bind } = params(p)?;
                let bind = bind.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
                if port < 1024 {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "use a port from 1024 to 65535",
                    ));
                }
                let fp = crate::remote_server::start(self.clone(), bind, port)
                    .await
                    .map_err(|e| err(ErrorCode::PortInUse, e))?;
                if let Ok(db) = self.db.lock() {
                    let _ = msm_storage::repo::set_setting(
                        &db,
                        "remote",
                        &json!({"enabled": true, "port": port, "bind": bind}),
                    );
                }
                Ok(
                    json!({"port": port, "fingerprint": msm_transport::remote::display_fingerprint(&fp)}),
                )
            }
            "remote.disable" => {
                self.remote.stop();
                if let Ok(db) = self.db.lock() {
                    let _ =
                        msm_storage::repo::set_setting(&db, "remote", &json!({"enabled": false}));
                }
                Ok(json!({"enabled": false}))
            }
            "pair.create_code" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    name: Option<String>,
                    #[serde(default)]
                    scopes: Option<Vec<msm_api::Capability>>,
                }
                let P { name, scopes } = params(p)?;
                if !self.remote.is_listening() {
                    return Err(err(
                        ErrorCode::InvalidStateTransition,
                        "turn on remote access first",
                    ));
                }
                let scopes = scopes.unwrap_or_else(crate::remote_server::default_scopes);
                let code = self
                    .remote
                    .issue_code(name.unwrap_or_else(|| "Controller".into()), scopes.clone());
                let fp = crate::remote_server::load_identity(self)
                    .map(|i| i.fingerprint())
                    .unwrap_or_default();
                Ok(json!({
                    "code": code,
                    "expires_in_secs": crate::remote_server::CODE_TTL.as_secs(),
                    "fingerprint": msm_transport::remote::display_fingerprint(&fp),
                    "scopes": scopes,
                }))
            }
            "devices.list" => {
                let db = self
                    .db
                    .lock()
                    .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
                let mut stmt = db
                    .prepare("SELECT id, name, cert_sha256, scopes_json, created_at, last_seen, revoked_at FROM paired_devices ORDER BY created_at")
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                let rows: Vec<Value> = stmt
                    .query_map([], |r| {
                        Ok(json!({
                            "id": r.get::<_, String>(0)?,
                            "name": r.get::<_, String>(1)?,
                            "fingerprint": msm_transport::remote::display_fingerprint(&r.get::<_, String>(2)?),
                            "scopes": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or_default(),
                            "created_at": r.get::<_, i64>(4)?,
                            "last_seen": r.get::<_, Option<i64>>(5)?,
                            "revoked": r.get::<_, Option<i64>>(6)?.is_some(),
                        }))
                    })
                    .and_then(Iterator::collect)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                Ok(json!(rows))
            }
            "devices.revoke" => {
                #[derive(Deserialize)]
                struct P {
                    device: msm_core::ids::DeviceId,
                }
                let P { device } = params(p)?;
                let n = {
                    let db = self
                        .db
                        .lock()
                        .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
                    db.execute("UPDATE paired_devices SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![now_ms(), device.to_string()])
                        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
                };
                if n == 0 {
                    return Err(err(ErrorCode::NotFound, "no such active device"));
                }
                self.remote.disconnect(device);
                self.relay_hub.close_device(device);
                Ok(json!({"revoked": device}))
            }
            c if c.starts_with("domains.") => self.domains(c, p).await,
            c if c.starts_with("relay.") => self.relay(principal, c, p).await,
            other => Err(err(
                ErrorCode::NotImplemented,
                format!("{other} is not implemented yet"),
            )),
        }
    }

    // ---- Relay (TUN-01) ------------------------------------------------------------------

    /// The relay hub (VPS side).
    pub fn relay_hub(&self) -> &crate::relay::RelayHub {
        &self.relay_hub
    }

    /// The relay client (home side).
    pub fn relay_client(&self) -> &crate::relay::RelayClient {
        &self.relay_client
    }

    /// A stored setting.
    pub fn setting(&self, key: &str) -> Option<Value> {
        let db = self.db.lock().ok()?;
        msm_storage::repo::get_setting(&db, key)
            .ok()
            .flatten()
            .filter(|v| !v.is_null())
    }

    /// Stores a setting.
    ///
    /// # Errors
    /// Storage errors.
    pub fn put_setting(&self, key: &str, value: &Value) -> Result<(), ApiError> {
        let db = self
            .db
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
        msm_storage::repo::set_setting(&db, key, value)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))
    }

    /// Ports used by servers on this machine.
    pub async fn local_instance_ports(&self) -> Vec<u16> {
        self.instances
            .read()
            .await
            .values()
            .map(|i| i.manifest.port)
            .collect()
    }

    /// A server's port, if it exists.
    pub async fn instance_port(&self, id: InstanceId) -> Option<u16> {
        self.instances
            .read()
            .await
            .get(&id)
            .map(|i| i.manifest.port)
    }

    /// A server's name, if it exists.
    pub async fn instance_name(&self, id: InstanceId) -> Option<String> {
        self.instances
            .read()
            .await
            .get(&id)
            .map(|i| i.manifest.name.clone())
    }

    fn relay_status_json(&self) -> Value {
        let Some(cfg) = crate::relay::load_config(self) else {
            return json!({ "configured": false });
        };
        let st = self.relay_client.status();
        let ports: Vec<Value> = cfg
            .ports
            .iter()
            .map(|(instance, port)| {
                json!({
                    "instance": instance,
                    "port": port,
                    "share_address": cfg.share_address(*port),
                    "open": st.ports.get(port).is_some_and(Option::is_none),
                    "error": st.ports.get(port).cloned().flatten(),
                })
            })
            .collect();
        json!({
            "configured": true,
            "address": cfg.address,
            "host": cfg.host(),
            "connected": st.connected,
            "last_error": st.last_error,
            "ports": ports,
            "active_connections": self.relay_client.active(),
        })
    }

    async fn relay(self: &Arc<Self>, principal: &Principal, cmd: &str, p: &Value) -> Res {
        use crate::relay::{ClientConfig, RelayClient, RelayHub, load_config, save_config};
        #[derive(Deserialize)]
        struct PortP {
            port: u16,
            #[serde(default)]
            label: String,
        }
        match cmd {
            // ---- on the VPS ----
            "relay.create_code" => {
                if !self.remote.is_listening() {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "remote access is off on this machine",
                    ));
                }
                let name = p["name"]
                    .as_str()
                    .unwrap_or("Home server")
                    .chars()
                    .take(48)
                    .collect::<String>();
                let code = self
                    .remote
                    .issue_code(format!("Relay: {name}"), vec![msm_api::Capability::Relay]);
                let identity = crate::remote_server::load_identity(self)
                    .map_err(|e| err(ErrorCode::Internal, e))?;
                Ok(
                    json!({ "code": code, "fingerprint": identity.fingerprint(), "port": self.remote_port_setting() }),
                )
            }
            "relay.open" => {
                let Principal::Device { id, .. } = principal else {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "only a paired home server can open relay ports",
                    ));
                };
                let PortP { port, label } = params(p)?;
                RelayHub::open(self, *id, port, label).await
            }
            "relay.close" => {
                let Principal::Device { id, .. } = principal else {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "only a paired home server can close relay ports",
                    ));
                };
                let PortP { port, .. } = params(p)?;
                Ok(json!({ "closed": self.relay_hub.close(*id, port) }))
            }
            "relay.hosted" => Ok(self.relay_hub.list()),

            // ---- at home ----
            "relay.status" => Ok(self.relay_status_json()),
            "relay.setup" => {
                #[derive(Deserialize)]
                struct P {
                    address: String,
                    fingerprint: String,
                    code: String,
                }
                let P {
                    address,
                    fingerprint,
                    code,
                } = params(p)?;
                let address = address.trim().to_owned();
                if !address.contains(':')
                    || address.chars().any(char::is_whitespace)
                    || address.len() > 255
                {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "the relay address must look like host:port",
                    ));
                }
                let device = crate::relay::pair(self, &address, &fingerprint, &code).await?;
                let ports = load_config(self).map(|c| c.ports).unwrap_or_default();
                save_config(
                    self,
                    Some(&ClientConfig {
                        address,
                        fingerprint,
                        device,
                        ports,
                    }),
                )?;
                RelayClient::restart(self);
                Ok(self.relay_status_json())
            }
            "relay.expose" | "relay.unexpose" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    port: Option<u16>,
                }
                let P { instance, port } = params(p)?;
                let inst = self.instance(instance).await?;
                let mut cfg = load_config(self)
                    .ok_or_else(|| err(ErrorCode::InvalidArgument, "set up the relay first"))?;
                if cmd == "relay.expose" {
                    let port = port.unwrap_or(inst.manifest.port);
                    if port < 1024 {
                        return Err(err(
                            ErrorCode::InvalidArgument,
                            "relay ports must be 1024 or higher",
                        ));
                    }
                    if cfg.ports.iter().any(|(i, p)| *i != instance && *p == port) {
                        return Err(err(
                            ErrorCode::InvalidArgument,
                            format!("port {port} is already used by another relayed server"),
                        ));
                    }
                    cfg.ports.insert(instance, port);
                } else {
                    cfg.ports.remove(&instance);
                }
                save_config(self, Some(&cfg))?;
                RelayClient::restart(self);
                // Give the control connection a moment so the answer can include the result.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                Ok(self.relay_status_json())
            }
            "relay.remove" => {
                save_config(self, None)?;
                RelayClient::restart(self);
                Ok(json!({ "configured": false }))
            }
            other => Err(err(
                ErrorCode::NotImplemented,
                format!("{other} is not implemented yet"),
            )),
        }
    }

    // ---- Domains (DNS-01..05) ----------------------------------------------------------

    fn cloudflare_token_path(&self) -> PathBuf {
        self.data.secrets().join("cloudflare.json")
    }

    fn cloudflare(&self) -> Option<msm_providers::cloudflare::Cloudflare> {
        let text = std::fs::read_to_string(self.cloudflare_token_path()).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let token = v["token"].as_str()?.to_owned();
        Some(msm_providers::cloudflare::Cloudflare::new(
            self.downloader.clone(),
            token,
        ))
    }

    fn dns_links(&self) -> serde_json::Map<String, Value> {
        self.db
            .lock()
            .ok()
            .and_then(|db| {
                msm_storage::repo::get_setting(&db, "dns_links")
                    .ok()
                    .flatten()
            })
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    fn save_dns_links(&self, links: serde_json::Map<String, Value>) -> Result<(), ApiError> {
        let db = self
            .db
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
        msm_storage::repo::set_setting(&db, "dns_links", &Value::Object(links))
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))
    }

    async fn public_ip_or_err(&self) -> Result<std::net::Ipv4Addr, ApiError> {
        msm_engine::network::public_ipv4(&self.downloader).await.ip.ok_or_else(|| {
            err(
                ErrorCode::UpstreamError,
                "this machine's public internet address couldn't be found; check the internet connection and try again",
            )
        })
    }

    /// Where a server's domain should point: the relay (VPS) when the server goes through one,
    /// otherwise this machine's public address. Returns the IPv4 and the public port.
    async fn domain_target(
        &self,
        instance: InstanceId,
        local_port: u16,
    ) -> Result<(std::net::Ipv4Addr, u16), ApiError> {
        if let Some(cfg) = crate::relay::load_config(self)
            && let Some(port) = cfg.ports.get(&instance).copied()
        {
            let host = cfg.host();
            let ip = match host.parse::<std::net::Ipv4Addr>() {
                Ok(ip) => ip,
                Err(_) => tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .ok()
                    .and_then(|mut it| {
                        it.find_map(|a| match a.ip() {
                            std::net::IpAddr::V4(v) => Some(v),
                            std::net::IpAddr::V6(_) => None,
                        })
                    })
                    .ok_or_else(|| {
                        err(
                            ErrorCode::UpstreamError,
                            format!("the relay address {host} couldn't be resolved"),
                        )
                    })?,
            };
            return Ok((ip, port));
        }
        Ok((self.public_ip_or_err().await?, local_port))
    }

    /// The public port friends use for a server (the relay port when relayed).
    fn public_port(&self, instance: InstanceId, local_port: u16) -> u16 {
        crate::relay::load_config(self)
            .and_then(|c| c.ports.get(&instance).copied())
            .unwrap_or(local_port)
    }

    fn cf_err(e: msm_providers::cloudflare::CloudflareError) -> ApiError {
        use msm_providers::cloudflare::CloudflareError as E;
        match e {
            E::Auth(m) => err(
                ErrorCode::Forbidden,
                format!(
                    "Cloudflare refused the token ({m}). Create a token with \"Edit zone DNS\" permission for your domain."
                ),
            ),
            E::Api(m) => err(ErrorCode::UpstreamError, format!("Cloudflare: {m}")),
            E::Http(h) => err(ErrorCode::UpstreamError, h.to_string()),
        }
    }

    /// Creates/updates the Cloudflare records for a link and returns the updated link.
    async fn apply_cloudflare_link(
        &self,
        cf: &msm_providers::cloudflare::Cloudflare,
        mut link: Value,
        ip: std::net::Ipv4Addr,
        port: u16,
    ) -> Result<Value, ApiError> {
        use msm_providers::cloudflare::{a_record, srv_record};
        let host = link["hostname"].as_str().unwrap_or_default().to_owned();
        let zone = link["zone_id"].as_str().unwrap_or_default().to_owned();
        let a_id = cf
            .upsert(&zone, &a_record(&host, ip))
            .await
            .map_err(Self::cf_err)?;
        link["a_id"] = json!(a_id);
        if port == msm_engine::domains::DEFAULT_PORT {
            if let Some(old) = link["srv_id"].as_str().map(str::to_owned) {
                cf.delete(&zone, &old).await.map_err(Self::cf_err)?;
            }
            link["srv_id"] = Value::Null;
        } else {
            let srv_id = cf
                .upsert(&zone, &srv_record(&host, port))
                .await
                .map_err(Self::cf_err)?;
            link["srv_id"] = json!(srv_id);
        }
        link["port"] = json!(port);
        link["last_ip"] = json!(ip.to_string());
        link["updated_at"] = json!(now_ms());
        link["last_error"] = Value::Null;
        Ok(link)
    }

    async fn domains(self: &Arc<Self>, cmd: &str, p: &Value) -> Res {
        use msm_engine::domains;
        match cmd {
            "domains.status" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let link = self.dns_links().get(&instance.to_string()).cloned();
                let share = link.as_ref().and_then(|l| l["hostname"].as_str()).map(|h| {
                    domains::share_address(h, self.public_port(instance, inst.manifest.port))
                });
                Ok(json!({
                    "cloudflare_connected": self.cloudflare_token_path().is_file(),
                    "link": link,
                    "share_address": share,
                }))
            }
            "domains.set_token" => {
                #[derive(Deserialize)]
                struct P {
                    token: String,
                }
                let P { token } = params(p)?;
                let token = token.trim().to_owned();
                if token.is_empty() || token.len() > 256 || token.chars().any(char::is_whitespace) {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        "that doesn't look like a Cloudflare API token",
                    ));
                }
                let cf = msm_providers::cloudflare::Cloudflare::new(
                    self.downloader.clone(),
                    token.clone(),
                );
                cf.verify().await.map_err(Self::cf_err)?;
                let zones = cf.zones().await.map_err(Self::cf_err)?;
                let path = self.cloudflare_token_path();
                msm_fsguard::write_atomic(&path, json!({ "token": token }).to_string().as_bytes())
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                Ok(json!({ "zones": zones }))
            }
            "domains.clear_token" => {
                let _ = std::fs::remove_file(self.cloudflare_token_path());
                Ok(json!({ "cleared": true }))
            }
            "domains.zones" => {
                let cf = self
                    .cloudflare()
                    .ok_or_else(|| err(ErrorCode::NotFound, "Cloudflare isn't connected"))?;
                Ok(json!({ "zones": cf.zones().await.map_err(Self::cf_err)? }))
            }
            "domains.link" => {
                #[derive(Deserialize)]
                struct P {
                    instance: InstanceId,
                    hostname: String,
                    provider: String,
                }
                let P {
                    instance,
                    hostname,
                    provider,
                } = params(p)?;
                let inst = self.instance(instance).await?;
                let host = domains::normalize_hostname(&hostname)
                    .map_err(|e| err(ErrorCode::InvalidArgument, e))?;
                let (ip, port) = self.domain_target(instance, inst.manifest.port).await?;
                let mut links = self.dns_links();
                if links.iter().any(|(k, v)| {
                    *k != instance.to_string() && v["hostname"].as_str() == Some(host.as_str())
                }) {
                    return Err(err(
                        ErrorCode::InvalidArgument,
                        format!("{host} is already linked to another server"),
                    ));
                }
                let link = match provider.as_str() {
                    "cloudflare" => {
                        let cf = self.cloudflare().ok_or_else(|| {
                            err(ErrorCode::InvalidArgument, "connect Cloudflare first")
                        })?;
                        let zones = cf.zones().await.map_err(Self::cf_err)?;
                        let names: Vec<String> = zones.iter().map(|z| z.name.clone()).collect();
                        let zone_name = domains::zone_for(&host, &names).cloned().ok_or_else(|| {
                            err(
                                ErrorCode::InvalidArgument,
                                format!("{host} isn't in a domain this Cloudflare token can edit ({})", names.join(", ")),
                            )
                        })?;
                        let zone = zones
                            .iter()
                            .find(|z| z.name == zone_name)
                            .map(|z| z.id.clone())
                            .unwrap_or_default();
                        // Keep the SRV record ID we made before for this name, so changes replace it.
                        let previous = links
                            .get(&instance.to_string())
                            .cloned()
                            .unwrap_or(Value::Null);
                        let srv_id = if previous["hostname"].as_str() == Some(host.as_str()) {
                            previous["srv_id"].clone()
                        } else {
                            Value::Null
                        };
                        let base = json!({
                            "hostname": host, "provider": "cloudflare", "zone_id": zone,
                            "zone_name": zone_name, "srv_id": srv_id,
                        });
                        self.apply_cloudflare_link(&cf, base, ip, port).await?
                    }
                    "manual" => json!({
                        "hostname": host, "provider": "manual", "port": port,
                        "last_ip": ip.to_string(), "updated_at": now_ms(),
                    }),
                    _ => {
                        return Err(err(
                            ErrorCode::InvalidArgument,
                            "provider must be cloudflare or manual",
                        ));
                    }
                };
                links.insert(instance.to_string(), link.clone());
                self.save_dns_links(links)?;
                Ok(json!({
                    "link": link,
                    "records": domains::planned_records(&host, ip, port),
                    "share_address": domains::share_address(&host, port),
                }))
            }
            "domains.records" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let link = self
                    .dns_links()
                    .get(&instance.to_string())
                    .cloned()
                    .ok_or_else(|| {
                        err(ErrorCode::NotFound, "no domain is linked to this server")
                    })?;
                let (ip, port) = self.domain_target(instance, inst.manifest.port).await?;
                let host = link["hostname"].as_str().unwrap_or_default();
                Ok(json!({ "records": domains::planned_records(host, ip, port) }))
            }
            "domains.unlink" => {
                let InstanceRef { instance } = params(p)?;
                let mut links = self.dns_links();
                let Some(link) = links.remove(&instance.to_string()) else {
                    return Ok(json!({ "unlinked": false }));
                };
                if link["provider"].as_str() == Some("cloudflare")
                    && let Some(cf) = self.cloudflare()
                {
                    let zone = link["zone_id"].as_str().unwrap_or_default();
                    for key in ["a_id", "srv_id"] {
                        if let Some(id) = link[key].as_str() {
                            cf.delete(zone, id).await.map_err(Self::cf_err)?;
                        }
                    }
                }
                self.save_dns_links(links)?;
                Ok(json!({ "unlinked": true }))
            }
            "domains.check" => {
                let InstanceRef { instance } = params(p)?;
                let inst = self.instance(instance).await?;
                let link = self
                    .dns_links()
                    .get(&instance.to_string())
                    .cloned()
                    .ok_or_else(|| {
                        err(ErrorCode::NotFound, "no domain is linked to this server")
                    })?;
                let host = link["hostname"].as_str().unwrap_or_default().to_owned();
                let srv_name = format!("_minecraft._tcp.{host}");
                let (target, a, srv) = tokio::join!(
                    self.domain_target(instance, inst.manifest.port),
                    msm_providers::cloudflare::lookup(&self.downloader, &host, "A"),
                    msm_providers::cloudflare::lookup(&self.downloader, &srv_name, "SRV"),
                );
                let a = a.map_err(|e| {
                    err(ErrorCode::UpstreamError, format!("DNS lookup failed: {e}"))
                })?;
                Ok(json!(domains::check(
                    &host,
                    target.as_ref().ok().map(|t| t.0),
                    a,
                    &srv.unwrap_or_default(),
                    target.map_or(inst.manifest.port, |t| t.1)
                )))
            }
            other => Err(err(
                ErrorCode::NotImplemented,
                format!("{other} is not implemented yet"),
            )),
        }
    }

    /// Keeps Cloudflare-linked domains pointing at this machine when its public IP changes
    /// (home connections often get a new address now and then). Runs every 10 minutes.
    pub fn spawn_dns_updater(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            loop {
                me.refresh_dns_links().await;
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        });
    }

    async fn refresh_dns_links(self: &Arc<Self>) {
        let links = self.dns_links();
        if !links
            .values()
            .any(|l| l["provider"].as_str() == Some("cloudflare"))
        {
            return;
        }
        let Some(cf) = self.cloudflare() else { return };
        let mut changed = false;
        let mut updated = links.clone();
        for (id, link) in &links {
            if link["provider"].as_str() != Some("cloudflare") {
                continue;
            }
            let Ok(instance) = id.parse::<InstanceId>() else {
                continue;
            };
            let Ok(inst) = self.instance(instance).await else {
                continue;
            };
            let Ok((ip, port)) = self.domain_target(instance, inst.manifest.port).await else {
                continue;
            };
            if link["last_ip"].as_str() == Some(ip.to_string().as_str())
                && link["port"].as_u64() == Some(u64::from(port))
            {
                continue;
            }
            let new = match self
                .apply_cloudflare_link(&cf, link.clone(), ip, port)
                .await
            {
                Ok(l) => {
                    self.audit_remote("system", "domains.auto_update", true);
                    l
                }
                Err(e) => {
                    let mut l = link.clone();
                    l["last_error"] = json!(e.detail);
                    l
                }
            };
            updated.insert(id.clone(), new);
            changed = true;
        }
        if changed {
            let _ = self.save_dns_links(updated);
        }
    }

    fn macros_for(&self, instance: Option<InstanceId>) -> Result<Vec<ActionDef>, ApiError> {
        let db = self
            .db
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
        let mut stmt = db
            .prepare(
                "SELECT def_json FROM command_macros WHERE instance_id IS NULL OR instance_id = ?1 ORDER BY name",
            )
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        let rows: Vec<String> = stmt
            .query_map(params![instance.map(|i| i.to_string())], |r| r.get(0))
            .and_then(Iterator::collect)
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|j| serde_json::from_str(j).ok())
            .collect())
    }

    fn find_action(&self, inst: &Instance, id: &str) -> Result<ActionDef, ApiError> {
        if let Some(a) = quick_commands::builtin_actions(&inst.manifest.mc_version)
            .into_iter()
            .find(|a| a.id == id)
        {
            return Ok(a);
        }
        self.macros_for(Some(inst.manifest.id))?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("command {id}")))
    }

    async fn moderate(&self, inst: Arc<Instance>, action: &Moderation, player: &str) -> Res {
        let name = PlayerName::parse(player)
            .map_err(|e| err(ErrorCode::InvalidArgument, e.to_string()))?;
        let _op = inst.op.lock().await;
        if let Some(proc) = inst.process() {
            let line = players::console_command(action, &name).map_err(ApiError::from)?;
            proc.send_line(&line)
                .await
                .map_err(|e| err(ErrorCode::ConsoleUnavailable, e.to_string()))?;
            return Ok(json!({"applied": "console", "command": line}));
        }
        if matches!(action, Moderation::Kick { .. }) {
            return Err(err(
                ErrorCode::InvalidStateTransition,
                "kick needs a running server",
            ));
        }
        // Stopped: edit the server's files, resolving the UUID the way the server would.
        let dir = inst.server_dir();
        let uuid = if let Some(u) = players::usercache_uuid(&dir, name.as_str()) {
            u
        } else if players::online_mode(&dir) {
            let id = msm_providers::mojang::profile_id(&self.downloader, name.as_str())
                .await
                .map_err(|e| ApiError::from(MsmError::from(e)))?
                .ok_or_else(|| {
                    err(
                        ErrorCode::NotFound,
                        format!("no Minecraft account named {name}"),
                    )
                })?;
            players::dashed_uuid(&id)
                .ok_or_else(|| err(ErrorCode::UpstreamError, "malformed UUID from Mojang"))?
        } else {
            players::offline_uuid(name.as_str())
        };
        players::apply_offline(&dir, action, &name, &uuid, now_ms() / 1000)
            .map_err(ApiError::from)?;
        Ok(json!({"applied": "file", "uuid": uuid}))
    }

    /// Returns an installed Java of exactly `major` (system or managed), installing Eclipse
    /// Temurin into the managed runtimes folder if none exists. Progress goes to `tasks`.
    async fn ensure_java(
        &self,
        task: TaskId,
        major: u32,
        cancel: &CancellationToken,
    ) -> Result<java::JavaInfo, ApiError> {
        let managed = [self.data.java_runtimes()];
        let found = java::discover(&managed).await;
        if let Some(rt) = java::select(&found, major, &managed) {
            return Ok(rt.clone());
        }
        self.events.publish(
            "tasks",
            json!({"task": task, "state": "active", "phase": "java", "java_major": major}),
        );
        let hub = &self.events;
        let last = std::sync::atomic::AtomicU64::new(u64::MAX);
        let progress = move |done: u64, total: Option<u64>| {
            let bucket = total
                .filter(|t| *t > 0)
                .map_or(done / (4 << 20), |t| done * 100 / t);
            if last.swap(bucket, std::sync::atomic::Ordering::Relaxed) != bucket {
                hub.publish("tasks", json!({"task": task, "state": "active", "phase": "java", "java_major": major, "progress": {"done": done, "total": total}}));
            }
        };
        msm_engine::java_install::ensure_temurin(
            &self.downloader,
            &self.data.java_runtimes(),
            &self.data.tmp_stage(),
            major,
            &progress,
            cancel,
        )
        .await
        .map_err(ApiError::from)
    }

    /// Takes a backup, pausing saving on a running server so the world on disk is consistent.
    async fn backup_now(
        &self,
        inst: &Arc<Instance>,
        scope: backups::Scope,
        reason: &str,
    ) -> Result<backups::BackupManifest, ApiError> {
        let proc = inst
            .process()
            .filter(|_| inst.state() == InstanceState::Running);
        let mut quiesced = false;
        if let Some(proc) = &proc {
            let (_, mut rx) = proc.subscribe(u64::MAX);
            proc.send_line("save-off")
                .await
                .map_err(|e| err(ErrorCode::ConsoleUnavailable, e.to_string()))?;
            proc.send_line("save-all flush")
                .await
                .map_err(|e| err(ErrorCode::ConsoleUnavailable, e.to_string()))?;
            quiesced = tokio::time::timeout(Duration::from_secs(120), async {
                while let Ok(line) = rx.recv().await {
                    if line.text.contains("Saved the game") {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
        }
        let id = msm_core::ids::BackupId::new().to_string();
        let (server, dest) = (inst.server_dir(), inst.dir.join("backups"));
        let (inst_id, mc) = (
            inst.manifest.id.to_string(),
            inst.manifest.mc_version.clone(),
        );
        let reason = reason.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            backups::create(
                &server,
                &dest,
                &backups::BackupRequest {
                    id: &id,
                    instance: &inst_id,
                    mc_version: &mc,
                    reason: &reason,
                    scope,
                    quiesced,
                },
            )
        })
        .await
        .map_err(|e| err(ErrorCode::Internal, e.to_string()));
        // Always turn saving back on, even if the backup failed.
        if let Some(proc) = &proc {
            let _ = proc.send_line("save-on").await;
        }
        result?.map_err(ApiError::from)
    }

    async fn install_content(
        &self,
        inst: &Arc<Instance>,
        kind: msm_providers::modrinth::ContentKind,
        target: &str,
        project: &str,
        cancel: &CancellationToken,
    ) -> Result<Value, ApiError> {
        let _op = inst.op.lock().await;
        let src = ModrinthSource {
            dl: self.downloader.clone(),
            kind,
            mc: inst.manifest.mc_version.clone(),
        };
        let server = inst.server_dir();
        let plan = msm_engine::content::plan(&src, project, &content::Registry::load(&server))
            .await
            .map_err(ApiError::from)?;
        if !plan.conflicts.is_empty() {
            return Err(err(ErrorCode::InvalidArgument, plan.conflicts.join("; ")));
        }
        if plan.install.is_empty() {
            return Ok(json!({"installed": [], "already_installed": plan.already_installed}));
        }
        // BAK-02: snapshot mods/plugins/config first so the change can be rolled back.
        let snapshot = self
            .backup_now(
                inst,
                backups::Scope::Content,
                "safety: before installing a mod",
            )
            .await?;
        let mut files = Vec::new();
        let progress = |_: u64, _: Option<u64>| {};
        for (idx, item) in plan.install.iter().enumerate() {
            let spec = item
                .download
                .clone()
                .ok_or_else(|| err(ErrorCode::Internal, "missing download"))?;
            let blob = self
                .downloader
                .fetch(&spec, &progress, cancel)
                .await
                .map_err(|e| ApiError::from(MsmError::from(e)))?;
            files.push((idx, blob.path));
        }
        let target = target.to_owned();
        let plan2 = plan.clone();
        tokio::task::spawn_blocking(move || content::apply(&server, &target, &plan2, &files))
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
            .map_err(ApiError::from)?;
        Ok(json!({
            "installed": plan.install.iter().map(|i| json!({"title": i.title, "version": i.version_number})).collect::<Vec<_>>(),
            "snapshot": snapshot.id,
            "restart_required": inst.process().is_some(),
        }))
    }

    /// Installs a Modrinth modpack as a new Fabric server. Everything is validated before the
    /// server is created; if anything fails afterwards the half-made server is moved to trash.
    async fn install_modpack(
        self: &Arc<Self>,
        task: TaskId,
        project: &str,
        name: String,
        port: u16,
        memory_mb: u32,
        cancel: &CancellationToken,
    ) -> Result<Value, ApiError> {
        let api = |e: msm_providers::http::DownloadError| ApiError::from(MsmError::from(e));
        let phase = |p: &str, extra: Value| {
            let mut v = json!({"task": task, "state": "active", "phase": p});
            if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
                o.extend(x.clone());
            }
            self.events.publish("tasks", v);
        };
        let progress = |_: u64, _: Option<u64>| {};

        // 1. The pack itself.
        let versions = msm_providers::modrinth::pack_versions(&self.downloader, project)
            .await
            .map_err(api)?;
        let version = versions
            .iter()
            .find(|v| v.version_type == "release")
            .or_else(|| versions.iter().find(|v| v.version_type == "beta"))
            .ok_or_else(|| {
                err(
                    ErrorCode::InvalidArgument,
                    "this modpack has no Fabric release",
                )
            })?
            .clone();
        let pack_blob = self
            .downloader
            .fetch(&version.download().map_err(api)?, &progress, cancel)
            .await
            .map_err(api)?;

        // 2. Validate everything before creating anything (PACK-02, PACK-04).
        let pack_path = pack_blob.path.clone();
        let index = tokio::task::spawn_blocking(move || msm_engine::mrpack::read_index(&pack_path))
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?
            .map_err(ApiError::from)?;
        let plan = msm_engine::mrpack::plan(&index).map_err(|problems| {
            err(
                ErrorCode::InvalidArgument,
                format!("this modpack can't be installed: {}", problems.join("; ")),
            )
        })?;

        // 3. The Fabric server it needs (Minecraft + pinned loader).
        phase("server", json!({"mc_version": plan.mc_version}));
        let vreq = VanillaRequest {
            software: vanilla::Software::Fabric,
            name,
            mc_version: plan.mc_version.clone(),
            port,
            memory_mb,
            allow_unsupported: false,
            fabric_loader: Some(plan.fabric_loader.clone()),
        };
        let dirs = InstallDirs {
            stage_root: self.data.tmp_stage(),
            instances_root: self.data.instances(),
        };
        let manifest = vanilla::install(
            &self.downloader,
            &self.mojang,
            &dirs,
            &vreq,
            &progress,
            cancel,
        )
        .await
        .map_err(|e| ApiError::from(MsmError::from(e)))?;
        let instance_dir = self.data.instance(manifest.id);
        let server_dir = instance_dir.join("server");

        let filled: Result<msm_engine::mrpack::PackLock, ApiError> = async {
            // 4. Pack files, each verified with SHA-512.
            let total = plan.files.len();
            let mut locked = Vec::new();
            for (n, f) in plan.files.iter().enumerate() {
                phase("files", json!({"progress": {"done": n, "total": total}}));
                let blob = self
                    .downloader
                    .fetch(&f.download, &progress, cancel)
                    .await
                    .map_err(api)?;
                let dest = f.path.join_onto(&server_dir);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                }
                std::fs::copy(&blob.path, &dest)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                if let msm_providers::http::Expected::Sha512(h) = &f.download.expected {
                    locked.push((f.path.to_string(), h.clone()));
                }
            }
            // 5. Overrides (safely extracted), then the lock file (PACK-03).
            phase("overrides", json!({}));
            let scratch = self.data.tmp_stage().join(format!("pack-{task}"));
            std::fs::create_dir_all(&scratch)
                .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
            let (pack, sd, sc) = (pack_blob.path.clone(), server_dir.clone(), scratch.clone());
            let applied = tokio::task::spawn_blocking(move || {
                msm_engine::mrpack::apply_overrides(&pack, &sc, &sd)
            })
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
            let _ = std::fs::remove_dir_all(&scratch);
            applied.map_err(ApiError::from)?;
            let lock = msm_engine::mrpack::PackLock {
                project_id: version.project_id.clone(),
                version_id: version.id.clone(),
                name: plan.name.clone(),
                pack_version: plan.version_id.clone(),
                mc_version: plan.mc_version.clone(),
                fabric_loader: plan.fabric_loader.clone(),
                mrpack_sha512: version
                    .primary_file()
                    .map(|f| f.hashes.sha512.clone())
                    .unwrap_or_default(),
                files: locked,
                skipped_client_only: plan.skipped_client_only.clone(),
            };
            msm_fsguard::write_atomic(
                &server_dir.join(msm_engine::mrpack::LOCK_FILE),
                &serde_json::to_vec_pretty(&lock)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?,
            )
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
            Ok(lock)
        }
        .await;
        let lock = match filled {
            Ok(l) => l,
            Err(e) => {
                // Never leave a half-built server behind: move it out of the way.
                let trash = self.data.root().join("trash");
                let _ = std::fs::create_dir_all(&trash);
                let _ = msm_fsguard::rename_with_retry(
                    &instance_dir,
                    &trash.join(format!("{}-failed-pack", manifest.id)),
                );
                return Err(e);
            }
        };
        let summary = self.register(manifest.clone()).await?;
        phase("java", json!({}));
        let java_error = self
            .ensure_java(task, manifest.java_major, cancel)
            .await
            .err();
        Ok(json!({
            "result": summary,
            "pack": {"name": lock.name, "version": lock.pack_version, "project": lock.project_id, "version_id": lock.version_id, "files": lock.files.len(), "skipped_client_only": lock.skipped_client_only.len()},
            "java_error": java_error,
        }))
    }

    /// Remote state.
    pub fn remote(&self) -> &crate::remote_server::RemoteState {
        &self.remote
    }

    /// Configured remote listen address, if remote access is enabled.
    pub fn remote_listen_setting(&self) -> Option<(std::net::IpAddr, u16)> {
        let db = self.db.lock().ok()?;
        let v = msm_storage::repo::get_setting(&db, "remote").ok()??;
        if v["enabled"].as_bool() != Some(true) {
            return None;
        }
        let port = v["port"].as_u64().and_then(|p| u16::try_from(p).ok())?;
        let bind = v["bind"]
            .as_str()
            .and_then(|b| b.parse().ok())
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        Some((bind, port))
    }

    /// Configured remote port, if remote access is enabled.
    pub fn remote_port_setting(&self) -> Option<u16> {
        self.remote_listen_setting().map(|(_, p)| p)
    }

    /// Looks up an active paired device by certificate fingerprint.
    pub fn device_by_fingerprint(
        &self,
        fp: &str,
    ) -> Option<(msm_core::ids::DeviceId, Vec<msm_api::Capability>)> {
        let db = self.db.lock().ok()?;
        let (id, scopes): (String, String) = db
            .query_row(
                "SELECT id, scopes_json FROM paired_devices WHERE cert_sha256 = ?1 AND revoked_at IS NULL",
                params![fp],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        Some((id.parse().ok()?, serde_json::from_str(&scopes).ok()?))
    }

    /// Records a newly paired device.
    pub fn add_device(
        &self,
        name: &str,
        fp: &str,
        scopes: &[msm_api::Capability],
    ) -> Result<msm_core::ids::DeviceId, ApiError> {
        let id = msm_core::ids::DeviceId::new();
        let db = self
            .db
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
        // A re-paired certificate replaces a previously revoked record.
        let _ = db.execute(
            "DELETE FROM paired_devices WHERE cert_sha256 = ?1 AND revoked_at IS NOT NULL",
            params![fp],
        );
        db.execute(
            "INSERT INTO paired_devices (id, name, cert_sha256, scopes_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.to_string(), name, fp, serde_json::to_string(scopes).unwrap_or_default(), now_ms()],
        )
        .map_err(|e| err(ErrorCode::AlreadyExists, e.to_string()))?;
        Ok(id)
    }

    /// Updates a device's last-seen time.
    pub fn touch_device(&self, id: msm_core::ids::DeviceId) {
        if let Ok(db) = self.db.lock() {
            let _ = db.execute(
                "UPDATE paired_devices SET last_seen = ?1 WHERE id = ?2",
                params![now_ms(), id.to_string()],
            );
        }
    }

    /// Audits a remote event that has no device principal yet (pairing).
    pub fn audit_remote(&self, who: &str, action: &str, ok: bool) {
        if let Ok(db) = self.db.lock() {
            let _ = append_audit(
                &db,
                &AuditEntry {
                    principal: who,
                    action,
                    target: None,
                    params: None,
                    outcome: if ok {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Error
                    },
                    error_code: (!ok).then_some(ErrorCode::Unauthenticated),
                },
            );
        }
    }

    async fn catalog(&self, software: vanilla::Software) -> Res {
        let to_api = |e: msm_providers::http::DownloadError| ApiError::from(MsmError::from(e));
        let manifest = self.mojang.manifest().await.map_err(to_api)?;
        // Offered = Mojang releases at or above the floor, intersected with what the loader
        // supports. Order follows Mojang's manifest (newest first), never string or semver order.
        let supported: Option<std::collections::HashSet<String>> = match software {
            vanilla::Software::Vanilla => None,
            vanilla::Software::Paper => Some(
                msm_providers::paper::versions(&self.downloader)
                    .await
                    .map_err(to_api)?
                    .into_iter()
                    .collect(),
            ),
            vanilla::Software::Fabric => Some(
                msm_providers::fabric::versions(&self.downloader)
                    .await
                    .map_err(to_api)?
                    .into_iter()
                    .collect(),
            ),
        };
        let versions: Vec<Value> = manifest
            .releases()
            .take_while(|v| {
                manifest.is_at_least(&v.id, vanilla::MIN_SUPPORTED_VERSION) == Some(true)
            })
            .filter(|v| supported.as_ref().is_none_or(|s| s.contains(&v.id)))
            .map(|v| json!({"software": software, "mc_version": v.id, "released": v.release_time}))
            .collect();
        let latest = versions
            .first()
            .and_then(|v| v["mc_version"].as_str())
            .unwrap_or_default()
            .to_owned();
        Ok(json!({"software": software, "latest": latest, "versions": versions}))
    }

    async fn port_available(&self, port: u16) -> Result<(), ApiError> {
        let taken = self
            .instances
            .read()
            .await
            .values()
            .any(|i| i.manifest.port == port);
        if taken {
            return Err(err(
                ErrorCode::PortInUse,
                format!("port {port} is used by another server"),
            ));
        }
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
            return Err(err(
                ErrorCode::PortInUse,
                format!("port {port} is in use by another program"),
            ));
        }
        Ok(())
    }

    async fn create(self: &Arc<Self>, req: CreateRequest) -> Res {
        self.port_available(req.port).await?;
        let task = TaskId::new();
        let cancel = CancellationToken::new();
        let record = json!({"task": task, "kind": "instances.create", "state": "active", "phase": "downloading"});
        if let Ok(mut t) = self.tasks.lock() {
            t.insert(task, (cancel.clone(), record.clone()));
        }
        self.events.publish("tasks", record);
        let me = self.clone();
        tokio::spawn(async move {
            let vreq = VanillaRequest {
                software: req.software,
                name: req.name,
                mc_version: req.mc_version,
                port: req.port,
                memory_mb: req.memory_mb,
                allow_unsupported: false,
                fabric_loader: None,
            };
            let dirs = InstallDirs {
                stage_root: me.data.tmp_stage(),
                instances_root: me.data.instances(),
            };
            let hub = &me.events;
            // Publish at most once per whole percent (or per 4 MiB when the size is unknown).
            let last = std::sync::atomic::AtomicU64::new(u64::MAX);
            let progress = move |done: u64, total: Option<u64>| {
                let bucket = total
                    .filter(|t| *t > 0)
                    .map_or(done / (4 << 20), |t| done * 100 / t);
                if last.swap(bucket, std::sync::atomic::Ordering::Relaxed) != bucket {
                    hub.publish("tasks", json!({"task": task, "state": "active", "phase": "downloading", "progress": {"done": done, "total": total}}));
                }
            };
            let result =
                vanilla::install(&me.downloader, &me.mojang, &dirs, &vreq, &progress, &cancel)
                    .await;
            let record = match result {
                Ok(manifest) => match me.register(manifest.clone()).await {
                    Ok(summary) => {
                        // Make sure a matching Java exists so the first start just works.
                        let java_error = me
                            .ensure_java(task, manifest.java_major, &cancel)
                            .await
                            .err();
                        json!({"task": task, "kind": "instances.create", "state": "completed", "result": summary, "java_error": java_error})
                    }
                    Err(e) => {
                        json!({"task": task, "kind": "instances.create", "state": "failed", "error": e})
                    }
                },
                Err(e) => {
                    let api: ApiError = MsmError::from(e).into();
                    let state = if api.code == ErrorCode::Cancelled {
                        "cancelled"
                    } else {
                        "failed"
                    };
                    json!({"task": task, "kind": "instances.create", "state": state, "error": api})
                }
            };
            if let Ok(mut t) = me.tasks.lock()
                && let Some(entry) = t.get_mut(&task)
            {
                entry.1 = record.clone();
            }
            me.events.publish("tasks", record);
        });
        Ok(json!({"task": task}))
    }

    async fn register(&self, manifest: InstanceManifest) -> Res {
        let dir = self.data.instance(manifest.id);
        {
            let db = self
                .db
                .lock()
                .map_err(|_| err(ErrorCode::Internal, "db lock"))?;
            db.execute(
                "INSERT INTO instances (id, name, software, mc_version, install_root, port, memory_json, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'stopped', ?8, ?8)",
                params![
                    manifest.id.to_string(),
                    manifest.name,
                    manifest.software,
                    manifest.mc_version,
                    dir.to_string_lossy(),
                    manifest.port,
                    json!({"max_mb": manifest.memory_mb}).to_string(),
                    now_ms(),
                ],
            )
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        }
        let inst = Arc::new(Instance {
            manifest,
            dir,
            state: Mutex::new(InstanceState::Stopped),
            process: Mutex::new(None),
            op: tokio::sync::Mutex::new(()),
            online: Mutex::new(OnlineTracker::default()),
            findings: Mutex::new(Vec::new()),
        });
        let summary = inst.summary();
        self.instances.write().await.insert(inst.manifest.id, inst);
        Ok(summary)
    }

    async fn start(self: &Arc<Self>, inst: Arc<Instance>) -> Res {
        let _op = inst.op.lock().await;
        if !vanilla::eula_accepted(&inst.dir) {
            return Err(err(
                ErrorCode::EulaNotAccepted,
                "the Minecraft EULA must be accepted before the first start",
            ));
        }
        if matches!(
            inst.state(),
            InstanceState::Starting | InstanceState::Running
        ) {
            return Ok(inst.summary());
        }
        let runtimes = java::discover(&[self.data.java_runtimes()]).await;
        let rt = java::select(
            &runtimes,
            inst.manifest.java_major,
            &[self.data.java_runtimes()],
        )
        .ok_or_else(|| {
            err(
                ErrorCode::JavaNotFound,
                format!(
                    "Java {} is required and was not found",
                    inst.manifest.java_major
                ),
            )
        })?
        .clone();
        if let Transition::NoOp(_) = self.apply(&inst, InstanceEvent::StartRequested)? {
            return Ok(inst.summary());
        }
        let spec = LaunchSpec {
            java: rt.executable,
            jvm_args: inst.manifest.jvm_args(),
            target: inst.manifest.target.clone(),
            server_args: inst.manifest.server_args.clone(),
            ready_pattern: inst.manifest.ready_pattern.clone(),
        };
        let proc = match self.launch_runner(&inst, spec).await {
            Ok(p) => p,
            Err(e) => {
                let _ = self.apply(&inst, InstanceEvent::ProcessExited { code: None });
                return Err(e);
            }
        };
        if let Ok(mut slot) = inst.process.lock() {
            *slot = Some(proc.clone());
        }
        if let Ok(mut f) = inst.findings.lock() {
            f.clear();
        }
        self.spawn_monitor(inst.clone(), proc);
        Ok(inst.summary())
    }

    fn spawn_monitor(self: &Arc<Self>, inst: Arc<Instance>, proc: Arc<RunnerClient>) {
        let topic = format!("instance/{}/console", inst.manifest.id);
        let players_topic = format!("instance/{}/players", inst.manifest.id);
        // Console forwarder (also feeds the online-player tracker).
        let me = self.clone();
        let p2 = proc.clone();
        let inst_c = inst.clone();
        tokio::spawn(async move {
            // Lines already buffered (e.g. replayed by a runner after a daemon restart) come
            // first; `last` de-duplicates lines that are both buffered and broadcast.
            let (backlog, mut rx) = p2.subscribe(0);
            let mut last = 0u64;
            let forward = |line: &msm_engine::process::ConsoleLine, last: &mut u64| {
                if line.seq != 0 && line.seq <= *last {
                    return;
                }
                *last = line.seq;
                let changed = inst_c
                    .online
                    .lock()
                    .map(|mut t| t.observe(&line.text, now_ms()))
                    .unwrap_or(false);
                me.events
                    .publish(&topic, json!({"text": line.text, "stderr": line.stderr}));
                if changed {
                    me.events
                        .publish(&players_topic, json!({"online": inst_c.online_players()}));
                }
            };
            for line in &backlog {
                forward(line, &mut last);
            }
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Ok(line) => forward(&line, &mut last),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            me.events.publish(&topic, json!({"dropped": n}));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = p2.wait_exit() => {
                        // Drain whatever is still queued, then stop.
                        while let Ok(line) = rx.try_recv() {
                            forward(&line, &mut last);
                        }
                        break;
                    }
                }
            }
        });
        // Lifecycle monitor.
        let me = self.clone();
        tokio::spawn(async move {
            let ready = proc.wait_ready(READY_TIMEOUT).await;
            if ready {
                let _ = me.apply(&inst, InstanceEvent::ReadyDetected);
            }
            let code = proc.wait_exit().await;
            let _ = me.apply(&inst, InstanceEvent::ProcessExited { code });
            if inst.state() == InstanceState::Crashed {
                let (lines, _) = proc.subscribe(0);
                let tail: Vec<String> = lines
                    .iter()
                    .rev()
                    .take(400)
                    .rev()
                    .map(|l| l.text.clone())
                    .collect();
                let findings =
                    msm_engine::diagnostics::analyze(&tail, code, inst.manifest.java_major);
                me.events.publish(
                    &format!("instance/{}/diagnostics", inst.manifest.id),
                    json!({"findings": findings}),
                );
                if let Ok(mut f) = inst.findings.lock() {
                    *f = findings;
                }
            }
            if let Ok(mut slot) = inst.process.lock() {
                *slot = None;
            }
            if let Ok(mut t) = inst.online.lock() {
                t.clear();
            }
            me.events.publish(
                &format!("instance/{}/players", inst.manifest.id),
                json!({"online": []}),
            );
        });
    }

    async fn stop(self: &Arc<Self>, inst: Arc<Instance>, timeout: Duration) -> Res {
        let _op = inst.op.lock().await;
        if let Transition::NoOp(_) = self.apply(&inst, InstanceEvent::StopRequested)? {
            return Ok(inst.summary());
        }
        let Some(proc) = inst.process() else {
            return Ok(inst.summary());
        };
        let me = self.clone();
        let inst2 = inst.clone();
        tokio::spawn(async move {
            if proc.stop(timeout).await.is_err() {
                let _ = me.apply(&inst2, InstanceEvent::StopTimedOut);
            }
        });
        Ok(inst.summary())
    }

    async fn kill(self: &Arc<Self>, inst: Arc<Instance>) -> Res {
        let _op = inst.op.lock().await;
        if let Transition::NoOp(_) = self.apply(&inst, InstanceEvent::ForceKillConfirmed)? {
            return Ok(inst.summary());
        }
        if let Some(proc) = inst.process() {
            proc.kill()
                .await
                .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        }
        Ok(inst.summary())
    }

    /// Starts `msmd runner` for an instance as a detached process and attaches to it.
    async fn launch_runner(
        &self,
        inst: &Instance,
        launch: LaunchSpec,
    ) -> Result<Arc<RunnerClient>, ApiError> {
        let internal = |e: String| err(ErrorCode::Internal, e);
        #[cfg(windows)]
        let endpoint = format!(
            "{}-runner-{}",
            msm_transport::windows_pipe::default_pipe_name()
                .map_err(|e| internal(e.to_string()))?,
            msm_core::ids::RequestId::new()
        );
        #[cfg(unix)]
        let endpoint = self
            .data
            .run()
            .join(format!(
                "r-{}.sock",
                &msm_core::ids::RequestId::new().to_string()[..12]
            ))
            .to_string_lossy()
            .into_owned();
        let info_path = inst.dir.join("runner.json");
        let _ = std::fs::remove_file(&info_path);
        let spec = RunnerSpec {
            launch,
            working_dir: inst.dir.join("server"),
            log_path: inst.dir.join("logs").join("console.log"),
            endpoint,
            info_path: info_path.clone(),
        };
        let spec_path = inst.dir.join("runner-spec.json");
        msm_fsguard::write_atomic(
            &spec_path,
            &serde_json::to_vec_pretty(&spec).map_err(|e| internal(e.to_string()))?,
        )
        .map_err(|e| internal(e.to_string()))?;
        let exe = std::env::current_exe().map_err(|e| internal(e.to_string()))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("runner")
            .arg(&spec_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            cmd.creation_flags(
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
            );
            if cmd.spawn().is_err() {
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
                cmd.spawn()
                    .map_err(|e| internal(format!("could not start the server process: {e}")))?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            cmd.spawn()
                .map_err(|e| internal(format!("could not start the server process: {e}")))?;
        }
        RunnerClient::attach_when_ready(&info_path, Duration::from_secs(30))
            .await
            .map_err(|e| internal(format!("the server process did not start: {e}")))
    }
}

#[derive(Debug, Deserialize)]
struct CreateRequest {
    #[serde(default)]
    software: vanilla::Software,
    name: String,
    mc_version: String,
    port: u16,
    memory_mb: u32,
}

/// Resolves a backup ID to its archive, refusing anything that is not a plain ULID-named file.
fn backup_path(inst: &Instance, backup: &str) -> Result<PathBuf, ApiError> {
    if backup.parse::<msm_core::ids::BackupId>().is_err() {
        return Err(err(ErrorCode::InvalidArgument, "invalid backup id"));
    }
    let path = inst.dir.join("backups").join(format!("{backup}.zip"));
    if !path.is_file() {
        return Err(err(ErrorCode::NotFound, format!("backup {backup}")));
    }
    Ok(path)
}

/// Which Modrinth content an instance accepts, and the folder it goes in.
fn content_kind(
    inst: &Instance,
) -> Result<(msm_providers::modrinth::ContentKind, &'static str), ApiError> {
    use msm_providers::modrinth::ContentKind;
    match inst.manifest.software.as_str() {
        "fabric" => Ok((ContentKind::FabricMod, "mods")),
        "paper" => Ok((ContentKind::PaperPlugin, "plugins")),
        _ => Err(err(
            ErrorCode::InvalidArgument,
            "Vanilla servers can't use mods or plugins. Create a Fabric server for mods or a Paper server for plugins.",
        )),
    }
}

/// Modrinth-backed version source for the content planner.
struct ModrinthSource {
    dl: Downloader,
    kind: msm_providers::modrinth::ContentKind,
    mc: String,
}

impl content::VersionSource for ModrinthSource {
    async fn versions(
        &self,
        project: &str,
    ) -> Result<Vec<msm_providers::modrinth::Version>, msm_providers::http::DownloadError> {
        msm_providers::modrinth::versions(&self.dl, project, self.kind.loaders(), &self.mc).await
    }
    async fn version(
        &self,
        id: &str,
    ) -> Result<msm_providers::modrinth::Version, msm_providers::http::DownloadError> {
        msm_providers::modrinth::version(&self.dl, id).await
    }
    async fn title(&self, project: &str) -> String {
        msm_providers::modrinth::project(&self.dl, project)
            .await
            .map_or_else(|_| project.to_owned(), |p| p.title)
    }
}

/// Adds or removes the inbound Windows Firewall rule for an instance's TCP port. Runs `netsh`
/// elevated through the standard Windows admin prompt; every argument is built from validated
/// values (a u16 port and a ULID), never from user text (NET-02, threat T16).
#[cfg(windows)]
async fn firewall_rule(inst: &Instance, allow: bool) -> Result<(), ApiError> {
    let name = format!("Minecraft server {} (MC Server Center)", inst.manifest.id);
    let netsh_args = if allow {
        format!(
            "advfirewall firewall add rule name=\"{name}\" dir=in action=allow protocol=TCP localport={} profile=any",
            inst.manifest.port
        )
    } else {
        format!("advfirewall firewall delete rule name=\"{name}\"")
    };
    // PowerShell single-quoted string: the content has no single quotes by construction.
    let script = format!(
        "$p = Start-Process -FilePath netsh -ArgumentList '{netsh_args}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode"
    );
    let status = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .await
        .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(err(
            ErrorCode::Forbidden,
            "Windows didn't allow the change (the admin prompt may have been declined)",
        ))
    }
}

#[cfg(not(windows))]
async fn firewall_rule(_inst: &Instance, _allow: bool) -> Result<(), ApiError> {
    Err(err(
        ErrorCode::NotImplemented,
        "on Linux, open the port with your firewall tool (for example: sudo ufw allow <port>/tcp)",
    ))
}
