// Typed access to the daemon. The webview never talks to the daemon directly: every call goes
// through the Tauri `daemon_request` command, and every check happens in the daemon.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type InstanceState =
  | "installing"
  | "install_failed"
  | "stopped"
  | "starting"
  | "running"
  | "stopping"
  | "stop_timeout"
  | "killing"
  | "crashed"
  | "reconciling"
  | "running_detached"
  | "unknown";

export type Software = "vanilla" | "paper" | "fabric";

export interface Instance {
  id: string;
  name: string;
  software: Software;
  build: string | null;
  mc_version: string;
  java_major: number;
  port: number;
  memory_mb: number;
  state: InstanceState;
  eula_accepted: boolean;
  pid: number | null;
}

export interface ApiError {
  code: string;
  message_key: string;
  detail: string;
  retryable: boolean;
}

export interface CatalogVersion {
  software: string;
  mc_version: string;
  released: string;
}

export interface ListedPlayer {
  name: string;
  uuid: string | null;
  reason?: string;
  level?: number;
}

export interface OnlinePlayer {
  name: string;
  uuid: string | null;
  joined_at_ms: number | null;
}

export interface PlayersInfo {
  running: boolean;
  online: OnlinePlayer[];
  lists: {
    ops: ListedPlayer[];
    whitelist: ListedPlayer[];
    banned: ListedPlayer[];
    banned_ips: ListedPlayer[];
    known: ListedPlayer[];
    whitelist_enabled: boolean;
  };
}

export type ParamType =
  | { kind: "player" }
  | { kind: "int"; min: number; max: number }
  | { kind: "choice"; options: string[] }
  | { kind: "bool" }
  | { kind: "word" }
  | { kind: "text"; max_len: number };

export type Param = { name: string; label: string; optional?: boolean } & ParamType;

export interface ActionDef {
  id: string;
  label: string;
  category: string;
  params: Param[];
  lines: string[];
}

export type ArgValue = string | number | boolean;

export interface UiEvent {
  host: string;
  topic: string;
  seq: number;
  data: unknown;
  gap: [number, number] | null;
}

export interface UiConnection {
  host: string;
  state: "connecting" | "starting" | "connected" | "disconnected" | "failed";
  detail: string | null;
}

export function isApiError(e: unknown): e is ApiError {
  return typeof e === "object" && e !== null && "code" in e && "detail" in e;
}

/** The host every call and subscription goes to: "local" (this PC) or a paired host ID. */
let currentHost = "local";
export const getHost = () => currentHost;
export function setHost(host: string) {
  currentHost = host;
}

export async function call<T>(cmd: string, params?: unknown): Promise<T> {
  return invoke<T>("daemon_request", { host: currentHost, cmd, params: params ?? null });
}

export async function subscribe(topic: string, fromSeq: number | null = 0): Promise<void> {
  await invoke("daemon_subscribe", { host: currentHost, topic, fromSeq });
}

/** Events for the currently selected host only. */
export function onEvent(handler: (e: UiEvent) => void): Promise<UnlistenFn> {
  return listen<UiEvent>("msm-event", (msg) => {
    if (msg.payload.host === currentHost) handler(msg.payload);
  });
}

export function onConnection(handler: (c: UiConnection) => void): Promise<UnlistenFn> {
  return listen<UiConnection>("msm-connection", (msg) => {
    if (msg.payload.host === currentHost) handler(msg.payload);
  });
}

export interface HostInfo {
  id: string;
  name: string;
  address: string | null;
}

export const hostsApi = {
  list: () => invoke<HostInfo[]>("list_hosts"),
  pair: (address: string, code: string, name: string) => invoke<HostInfo>("pair_host", { address, code, name }),
  remove: (id: string) => invoke("remove_host", { id }),
};

export const api = {
  instances: () => call<Instance[]>("instances.list"),
  versions: (software: Software) => call<{ latest: string; versions: CatalogVersion[] }>("catalog.list", { software }),
  create: (p: { software: Software; name: string; mc_version: string; port: number; memory_mb: number }) =>
    call<{ task: string }>("instances.create", p),
  acceptEula: (instance: string) => call<Instance>("instances.accept_eula", { instance, accept: true }),
  start: (instance: string) => call<Instance>("instances.start", { instance }),
  stop: (instance: string) => call<Instance>("instances.stop", { instance }),
  kill: (instance: string) => call<Instance>("instances.kill", { instance, confirm: true }),
  send: (instance: string, line: string) => call("console.send", { instance, line }),
  network: () => call<{ lan_ipv4: string | null; lan_kind: string | null }>("network.info"),
  installJava: (major: number) => call<{ task: string }>("java.install", { major }),
  players: (instance: string) => call<PlayersInfo>("players.list", { instance }),
  moderate: (action: string, instance: string, player: string, reason?: string) =>
    call<{ applied: "console" | "file" }>(`players.${action}`, { instance, player, reason: reason || null }),
  setWhitelist: (instance: string, enabled: boolean) =>
    call("players.whitelist_set_enabled", { instance, enabled }),
  commands: (instance: string) => call<{ builtin: ActionDef[]; macros: ActionDef[] }>("commands.catalog", { instance }),
  preview: (instance: string, action: string, args: Record<string, ArgValue>) =>
    call<{ lines: string[] }>("commands.preview", { instance, action, args }),
  run: (instance: string, action: string, args: Record<string, ArgValue>) =>
    call<{ sent: string[] }>("commands.run", { instance, action, args }),
  saveMacro: (instance: string | null, def: ActionDef) => call<ActionDef>("macros.save", { instance, macro: def }),
  deleteMacro: (id: string) => call("macros.delete", { id }),
};
