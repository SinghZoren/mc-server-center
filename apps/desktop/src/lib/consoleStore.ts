// App-wide console buffers, so switching tabs or servers never loses output. One daemon
// subscription per server; lines are kept here and rendered as plain text by the Console tab.
import { useSyncExternalStore } from "react";
import { onEvent, subscribe } from "./api";

export interface ConsoleLine {
  seq: number;
  text: string;
  kind: "out" | "err" | "gap" | "me";
}

const MAX_LINES = 2000;
const buffers = new Map<string, ConsoleLine[]>();
const listeners = new Map<string, Set<() => void>>();
const started = new Set<string>();
let globalListener: Promise<unknown> | null = null;

function emit(id: string) {
  listeners.get(id)?.forEach((l) => l());
}

export function pushLine(id: string, line: ConsoleLine) {
  const prev = buffers.get(id) ?? [];
  const next = prev.length >= MAX_LINES ? [...prev.slice(-MAX_LINES + 1), line] : [...prev, line];
  buffers.set(id, next);
  emit(id);
}

function ensure(id: string) {
  if (!globalListener) {
    globalListener = onEvent((e) => {
      const m = /^instance\/([^/]+)\/console$/.exec(e.topic);
      if (!m) return;
      const sid = m[1]!;
      if (e.gap) {
        pushLine(sid, { seq: e.seq, text: "… earlier output is no longer available …", kind: "gap" });
        return;
      }
      const d = e.data as { text?: string; stderr?: boolean; dropped?: number } | null;
      if (typeof d?.text === "string") pushLine(sid, { seq: e.seq, text: d.text, kind: d.stderr ? "err" : "out" });
      else if (d?.dropped) pushLine(sid, { seq: e.seq, text: `… ${d.dropped} lines skipped …`, kind: "gap" });
    });
  }
  if (!started.has(id)) {
    started.add(id);
    void globalListener.then(() => subscribe(`instance/${id}/console`, 0));
  }
}

const EMPTY: ConsoleLine[] = [];

export function useConsole(id: string): ConsoleLine[] {
  ensure(id);
  return useSyncExternalStore(
    (cb) => {
      const set = listeners.get(id) ?? new Set();
      set.add(cb);
      listeners.set(id, set);
      return () => set.delete(cb);
    },
    () => buffers.get(id) ?? EMPTY,
  );
}
