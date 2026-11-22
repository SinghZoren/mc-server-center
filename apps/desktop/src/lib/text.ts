// User-facing strings live here so the UI can be translated later (English for now).
import type { ApiError, InstanceState } from "./api";
import { isApiError } from "./api";

export const stateLabel: Record<InstanceState, string> = {
  installing: "Installing",
  install_failed: "Install failed",
  stopped: "Stopped",
  starting: "Starting",
  running: "Online",
  stopping: "Stopping",
  stop_timeout: "Not responding to stop",
  killing: "Force stopping",
  crashed: "Crashed",
  reconciling: "Checking",
  running_detached: "Running (console detached)",
  unknown: "Unknown — check Task Manager",
};

export function stateTone(s: InstanceState): "live" | "busy" | "off" | "bad" {
  if (s === "running") return "live";
  if (s === "starting" || s === "stopping" || s === "installing" || s === "reconciling" || s === "killing") return "busy";
  if (s === "crashed" || s === "install_failed" || s === "stop_timeout" || s === "unknown") return "bad";
  return "off";
}

const friendly: Record<string, string> = {
  EULA_NOT_ACCEPTED: "You need to accept the Minecraft EULA before the first start.",
  PORT_IN_USE: "That port is already in use. Pick another one.",
  JAVA_NOT_FOUND: "The right version of Java isn't installed yet.",
  CONSOLE_UNAVAILABLE: "The server isn't running, so it can't receive commands.",
  INVALID_STATE_TRANSITION: "That isn't possible right now.",
  DOWNLOAD_HASH_MISMATCH: "A download was corrupted and was thrown away. Try again.",
  DOWNLOAD_HOST_NOT_ALLOWED: "A download pointed somewhere that isn't trusted, so it was blocked.",
  UPSTREAM_ERROR: "A download service didn't respond properly. Check your internet and try again.",
  RATE_LIMITED: "A download service asked us to slow down. Try again in a minute.",
  NOT_FOUND: "That couldn't be found.",
  FORBIDDEN: "You don't have permission to do that.",
  CANCELLED: "Cancelled.",
  NOT_IMPLEMENTED: "That isn't available yet.",
};

export function errorText(e: unknown): string {
  if (isApiError(e)) {
    if (e.detail?.startsWith("unknown command")) {
      return "The background service is older than this app, so this isn't available. Quit the app from the tray, end \"msmd.exe\" in Task Manager, then open the app again.";
    }
    const base = friendly[e.code];
    return base ? `${base}` : e.detail || e.code;
  }
  return e instanceof Error ? e.message : String(e);
}

export function errorDetail(e: unknown): string | null {
  return isApiError(e) ? `${e.code}: ${e.detail}` : null;
}

export type { ApiError };

export function formatMemory(mb: number): string {
  return mb >= 1024 ? `${(mb / 1024).toFixed(mb % 1024 === 0 ? 0 : 1)} GB` : `${mb} MB`;
}

export function since(ms: number | null): string {
  if (!ms) return "";
  const m = Math.max(0, Math.round((Date.now() - ms) / 60000));
  if (m < 1) return "just now";
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}
