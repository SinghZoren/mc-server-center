import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useQuery } from "@tanstack/react-query";
import { api, getHost, setHost, type Instance } from "./lib/api";
import { HostSwitcher } from "./components/HostSwitcher";
import { useQueryClient } from "@tanstack/react-query";
import { useConnection, useLiveInstances } from "./lib/hooks";
import { formatMemory } from "./lib/text";
import { EmptyState, Notice, Spinner, StatusPill } from "./components/ui";
import { CreateServer } from "./views/CreateServer";
import { ServerView } from "./views/ServerView";

export function App() {
  const [host, setHostState] = useState(getHost());
  const [quitting, setQuitting] = useState(false);
  const qc = useQueryClient();
  useEffect(() => {
    let off: (() => void) | undefined;
    void listen("msm-quitting", () => setQuitting(true)).then((u) => (off = u));
    return () => off?.();
  }, []);
  if (quitting) {
    return (
      <div className="grid h-full place-items-center p-8 text-center" role="status">
        <div className="space-y-3">
          <Spinner label="Quitting" />
          <p className="font-display text-lg font-semibold">Saving worlds and stopping servers…</p>
          <p className="text-sm text-muted">MC Server Center closes by itself when everything has stopped. This can take up to a minute.</p>
        </div>
      </div>
    );
  }
  return (
    <HostScope
      key={host}
      host={host}
      onHostChange={(h) => {
        setHost(h);
        qc.clear();
        setHostState(h);
      }}
    />
  );
}

function HostScope({ host, onHostChange }: { host: string; onHostChange: (h: string) => void }) {
  const connection = useConnection();
  const instances = useQuery({ queryKey: ["instances"], queryFn: api.instances, retry: 1 });
  const list = instances.data ?? [];
  useLiveInstances(list.map((i) => i.id));
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  // Retry the initial load once the service connection comes up.
  useEffect(() => {
    if (connection.state === "connected") void instances.refetch();
  }, [connection.state]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!selected && list.length > 0) setSelected(list[0]!.id);
  }, [list, selected]);

  // If the selected server was deleted, fall back to the first one.
  useEffect(() => {
    if (selected && instances.isSuccess && !list.some((i) => i.id === selected)) setSelected(list[0]?.id ?? null);
  }, [list, selected, instances.isSuccess]);

  const current = list.find((i) => i.id === selected) ?? null;

  return (
    <div className="flex h-full">
      <aside className="flex w-72 shrink-0 flex-col border-r border-line bg-panel/70" aria-label="Your servers">
        <div className="px-5 pt-6 pb-4">
          <div className="flex items-center gap-2.5">
            <span className="grid grid-cols-2 gap-[3px]" aria-hidden>
              <span className="h-2.5 w-2.5 bg-ember" />
              <span className="h-2.5 w-2.5 bg-ember-deep" />
              <span className="h-2.5 w-2.5 bg-ember-deep" />
              <span className="h-2.5 w-2.5 bg-ember" />
            </span>
            <span className="font-display text-lg font-semibold tracking-tight">MC Server Center</span>
          </div>
        </div>
        <HostSwitcher host={host} onChange={onHostChange} />
        <div className="flex items-center justify-between px-5 pb-2">
          <span className="label mb-0">My servers</span>
          <button className="btn btn-ghost px-2 py-1 text-ember" onClick={() => setCreating(true)} aria-label="Create a server">
            + New
          </button>
        </div>
        <nav className="flex-1 space-y-2 overflow-y-auto px-3 pb-4">
          {list.map((inst, i) => (
            <ServerTab key={inst.id} inst={inst} active={inst.id === selected} index={i} onClick={() => setSelected(inst.id)} />
          ))}
          {instances.isSuccess && list.length === 0 && <p className="px-2 py-3 text-sm text-faint">No servers yet.</p>}
        </nav>
        <ConnectionBar state={connection.state} detail={connection.detail} />
      </aside>

      <main className="relative flex flex-1 flex-col overflow-hidden">
        {connection.state === "outdated" && (
          <div className="border-b border-line px-10 py-3">
            <Notice tone="warn">{connection.detail}</Notice>
          </div>
        )}
        <div className="relative min-h-0 flex-1">
        {instances.isPending && connection.state !== "disconnected" ? (
          <div className="grid h-full place-items-center">
            <Spinner label="Connecting to the service" />
          </div>
        ) : current ? (
          <ServerView key={current.id} inst={current} />
        ) : (
          <div className="h-full overflow-y-auto px-10">
            {instances.isError && connection.state !== "connected" ? (
              <div className="pt-16">
                <Notice tone="error">The background service isn't reachable yet. {connection.detail}</Notice>
              </div>
            ) : (
              <EmptyState
                title="Host a world for your friends"
                action={
                  <button className="btn btn-primary mt-2" onClick={() => setCreating(true)}>
                    Create your first server
                  </button>
                }
              >
                Pick a Minecraft version and we'll download it, check it, and set it up. No terminal and no manual Java
                setup.
              </EmptyState>
            )}
          </div>
        )}
        </div>
      </main>

      {creating && (
        <CreateServer
          onClose={() => setCreating(false)}
          onCreated={(id) => {
            setCreating(false);
            setSelected(id);
          }}
        />
      )}
    </div>
  );
}

function ServerTab({ inst, active, index, onClick }: { inst: Instance; active: boolean; index: number; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={`rise block w-full border px-3.5 py-3 text-left transition-colors ${
        active ? "border-ember/60 bg-panel-2 shadow-[3px_3px_0_#08090b]" : "border-transparent hover:bg-panel-2/60"
      }`}
      style={{ animationDelay: `${index * 40}ms` }}
    >
      <div className="truncate font-semibold">{inst.name}</div>
      <div className="mt-1 flex items-center justify-between gap-2">
        <StatusPill state={inst.state} />
        <span className="console text-[10px] text-faint">
          {inst.mc_version} · {formatMemory(inst.memory_mb)}
        </span>
      </div>
    </button>
  );
}

function ConnectionBar({ state, detail }: { state: string; detail: string | null }) {
  const text =
    state === "connected"
      ? "Service running"
      : state === "outdated"
        ? "Old service running"
        : state === "starting"
          ? "Starting service…"
          : state === "connecting"
            ? "Connecting…"
            : "Service offline";
  const color = state === "connected" ? "var(--color-moss)" : state === "disconnected" || state === "failed" ? "var(--color-danger)" : "var(--color-amber)";
  return (
    <div className="border-t border-line px-5 py-3 text-xs text-muted" role="status" title={detail ?? undefined}>
      <span className="mr-2 inline-block h-1.5 w-1.5" style={{ background: color }} />
      {text}
      <p className="mt-1 text-[11px] text-faint">Closing the window saves and stops your servers, then quits.</p>
    </div>
  );
}
