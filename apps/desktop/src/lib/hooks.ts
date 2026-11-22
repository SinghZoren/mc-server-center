import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { connectionStatus, getHost, onConnection, onEvent, subscribe, type UiConnection, type UiEvent } from "./api";

/** Calls `handler` for every daemon event whose topic matches `topic` exactly. Subscribes once. */
export function useTopic(topic: string | null, handler: (e: UiEvent) => void, fromSeq: number | null = 0) {
  const ref = useRef(handler);
  ref.current = handler;
  useEffect(() => {
    if (!topic) return;
    let unlisten: (() => void) | undefined;
    let live = true;
    void onEvent((e) => {
      if (e.topic === topic) ref.current(e);
    }).then((u) => {
      if (live) {
        unlisten = u;
        void subscribe(topic, fromSeq);
      } else u();
    });
    return () => {
      live = false;
      unlisten?.();
    };
  }, [topic, fromSeq]);
}

/** Keeps the instance list fresh when any server changes state or an install finishes. */
export function useLiveInstances(ids: string[]) {
  const qc = useQueryClient();
  const key = ids.join(",");
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void onEvent((e) => {
      if (e.topic.endsWith("/state") || e.topic === "tasks") {
        void qc.invalidateQueries({ queryKey: ["instances"] });
      }
    }).then((u) => (unlisten = u));
    void subscribe("tasks", null);
    for (const id of ids) void subscribe(`instance/${id}/state`, null);
    return () => unlisten?.();
  }, [key, qc]);
}

export function useConnection(): UiConnection {
  const [c, setC] = useState<UiConnection>({ host: getHost(), state: "connecting", detail: null });
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let live = true;
    void onConnection(setC).then((u) => (unlisten = u));
    // State events only fire on changes; ask for the current one too.
    void connectionStatus()
      .then((s) => live && s && setC((prev) => (prev.state === "connecting" ? s : prev)))
      .catch(() => undefined);
    return () => {
      live = false;
      unlisten?.();
    };
  }, []);
  return c;
}
