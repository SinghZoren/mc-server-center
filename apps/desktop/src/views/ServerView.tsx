import { useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, call, isApiError, type Instance } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorText, formatMemory } from "../lib/text";
import { Confirm, Notice, Spinner, StatusPill } from "../components/ui";
import { ConsoleTab } from "./ConsoleTab";
import { PlayersTab } from "./PlayersTab";
import { CommandsTab } from "./CommandsTab";
import { BackupsTab } from "./BackupsTab";
import { SettingsTab } from "./SettingsTab";
import { ContentTab } from "./ContentTab";
import { ConnectWizard } from "./ConnectWizard";

const TABS = ["Overview", "Console", "Players", "Quick commands", "Mods", "Settings", "Backups"] as const;
type Tab = (typeof TABS)[number];

export function ServerView({ inst }: { inst: Instance }) {
  const [tab, setTab] = useState<Tab>("Overview");
  return (
    <div className="flex h-full flex-col">
      <header className="border-b border-line px-10 pt-8">
        <div className="flex items-end justify-between gap-6">
          <div className="rise min-w-0">
            <p className="label">
              Minecraft {inst.mc_version} · {{ vanilla: "Vanilla", paper: "Paper", fabric: "Fabric" }[inst.software] ?? inst.software}
              {inst.build ? ` · ${inst.build}` : ""}
            </p>
            <h1 className="font-display truncate text-4xl leading-tight font-semibold tracking-tight">{inst.name}</h1>
            <div className="mt-2">
              <StatusPill state={inst.state} large />
            </div>
          </div>
          <PowerControls inst={inst} />
        </div>
        <nav className="mt-6 flex gap-1" role="tablist" aria-label="Server sections">
          {TABS.map((t) => (
            <button
              key={t}
              role="tab"
              aria-selected={tab === t}
              onClick={() => setTab(t)}
              className={`-mb-px border-b-2 px-4 py-2.5 text-sm font-semibold transition-colors ${
                tab === t ? "border-ember text-ink" : "border-transparent text-muted hover:text-ink"
              }`}
            >
              {t === "Mods" && inst.software === "paper" ? "Plugins" : t}
            </button>
          ))}
        </nav>
      </header>
      <section className="min-h-0 flex-1 overflow-y-auto px-10 py-8" role="tabpanel" aria-label={tab}>
        {tab === "Overview" && <Overview inst={inst} />}
        {tab === "Console" && <ConsoleTab inst={inst} />}
        {tab === "Players" && <PlayersTab inst={inst} />}
        {tab === "Quick commands" && <CommandsTab inst={inst} />}
        {tab === "Mods" && <ContentTab inst={inst} />}
        {tab === "Settings" && <SettingsTab inst={inst} />}
        {tab === "Backups" && <BackupsTab inst={inst} />}
      </section>
    </div>
  );
}

function PowerControls({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [confirm, setConfirm] = useState<"eula" | "kill" | null>(null);
  const [agreed, setAgreed] = useState(false);
  const [javaTask, setJavaTask] = useState<string | null>(null);
  const [javaProgress, setJavaProgress] = useState<number | null>(null);

  useTopic(javaTask ? "tasks" : null, (e) => {
    const d = e.data as { task?: string; state?: string; progress?: { done: number; total: number | null } };
    if (d?.task !== javaTask) return;
    if (d.progress?.total) setJavaProgress(Math.round((d.progress.done / d.progress.total) * 100));
    if (d.state === "completed") {
      setJavaTask(null);
      setError(null);
      void act(() => api.start(inst.id));
    } else if (d.state === "failed") {
      setJavaTask(null);
      setError(d);
    }
  }, null);

  const needsJava = isApiError(error) && error.code === "JAVA_NOT_FOUND";

  async function act(fn: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
      void qc.invalidateQueries({ queryKey: ["instances"] });
    }
  }

  const s = inst.state;
  const canStart = s === "stopped" || s === "crashed";
  const canStop = s === "running" || s === "starting";
  return (
    <div className="flex flex-col items-end gap-2">
      <div className="flex gap-3">
        {canStart && (
          <button
            className="btn btn-primary px-5 py-2.5 text-sm"
            disabled={busy}
            onClick={() => (inst.eula_accepted ? act(() => api.start(inst.id)) : setConfirm("eula"))}
          >
            {busy ? <Spinner /> : "▶ Start"}
          </button>
        )}
        {canStop && (
          <button className="btn px-5 py-2.5" disabled={busy} onClick={() => act(() => api.stop(inst.id))}>
            ■ Stop
          </button>
        )}
        {(s === "stop_timeout" || s === "stopping") && (
          <button className="btn btn-danger" onClick={() => setConfirm("kill")}>
            Force stop…
          </button>
        )}
      </div>
      {error != null && <p className="max-w-sm text-right text-xs text-danger" role="alert">{errorText(error)}</p>}
      {needsJava && !javaTask && (
        <button
          className="btn btn-primary"
          onClick={async () => {
            try {
              const { task } = await api.installJava(inst.java_major);
              setJavaProgress(null);
              setJavaTask(task);
            } catch (e) {
              setError(e);
            }
          }}
        >
          Install Java {inst.java_major} and start
        </button>
      )}
      {javaTask && (
        <p className="console text-xs text-muted" aria-live="polite">
          Installing Java {inst.java_major}… {javaProgress != null ? `${javaProgress}%` : ""}
        </p>
      )}

      {confirm === "eula" && (
        <Confirm
          title="Accept the Minecraft EULA"
          body={
            <>
              <p>Minecraft servers require you to accept Mojang's End User License Agreement before the first start.</p>
              <p className="console border border-line bg-[#0f1013] px-3 py-2 text-xs select-all">https://aka.ms/MinecraftEULA</p>
            </>
          }
          confirmLabel="Accept and start"
          onClose={() => setConfirm(null)}
          onConfirm={() => {
            if (!agreed) return;
            setConfirm(null);
            void act(async () => {
              await api.acceptEula(inst.id);
              await api.start(inst.id);
            });
          }}
        >
          <label className="flex cursor-pointer items-start gap-3 text-sm">
            <input type="checkbox" className="mt-0.5 h-4 w-4 accent-[var(--color-ember)]" checked={agreed} onChange={(e) => setAgreed(e.target.checked)} />
            <span>I have read and agree to the Minecraft EULA.</span>
          </label>
        </Confirm>
      )}
      {confirm === "kill" && (
        <Confirm
          title="Force stop the server?"
          danger
          body={
            <>
              <p>The server isn't responding to a normal stop. Forcing it closes it immediately.</p>
              <p className="text-amber">Anything that happened since the last autosave (up to a few minutes) may be lost.</p>
            </>
          }
          confirmLabel="Force stop"
          onClose={() => setConfirm(null)}
          onConfirm={() => {
            setConfirm(null);
            void act(() => api.kill(inst.id));
          }}
        />
      )}
    </div>
  );
}

interface Finding {
  code: string;
  severity: "error" | "warning";
  title: string;
  advice: string;
  evidence: string | null;
  repair: { kind: "install_java"; major: number } | { kind: "accept_eula" | "adjust_memory" | "change_port" | "restore_backup" } | null;
}

const REPAIR_HINT: Record<string, string> = {
  accept_eula: "Press Start to review and accept the EULA.",
  adjust_memory: "Change the memory in the Manage section below.",
  change_port: "Change the port by stopping the other program, or create the server on a free port.",
  restore_backup: "Open the Backups tab to restore a working copy.",
};

function Diagnosis({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["diagnostics", inst.id];
  const q = useQuery({ queryKey: key, queryFn: () => call<{ findings: Finding[] }>("diagnostics.list", { instance: inst.id }) });
  useTopic(`instance/${inst.id}/diagnostics`, () => void qc.invalidateQueries({ queryKey: key }), null);
  const [note, setNote] = useState<string | null>(null);
  const findings = q.data?.findings ?? [];
  return (
    <div className="mt-5 space-y-3" aria-live="polite">
      {findings.length === 0 && <Notice tone="error">The server stopped unexpectedly. The Console tab shows its last messages.</Notice>}
      {findings.map((f) => (
        <div key={f.code} className="border-l-2 border-danger bg-panel-2/60 px-4 py-3">
          <div className="flex items-baseline justify-between gap-3">
            <strong className="text-sm">{f.title}</strong>
            <span className="console text-[10px] text-faint">{f.code}</span>
          </div>
          <p className="mt-1 text-sm text-ink/85">{f.advice}</p>
          {f.evidence && <p className="console mt-2 truncate text-[11px] text-faint" title={f.evidence}>{f.evidence}</p>}
          {f.repair?.kind === "install_java" ? (
            <button
              className="btn btn-primary mt-3 text-xs"
              onClick={async () => {
                const major = (f.repair as { major: number }).major;
                try {
                  await api.installJava(major);
                  setNote(`Installing Java ${major}… The server will use it on its next start.`);
                } catch (e) {
                  setNote(errorText(e));
                }
              }}
            >
              Install Java {(f.repair as { major: number }).major}
            </button>
          ) : f.repair ? (
            <p className="mt-2 text-xs text-muted">{REPAIR_HINT[f.repair.kind]}</p>
          ) : null}
        </div>
      ))}
      {note && <p className="text-xs text-muted">{note}</p>}
    </div>
  );
}

function JoinRow({ who, address, children }: { who: string; address: string; children?: ReactNode }) {
  const [copied, setCopied] = useState(false);
  return (
    <li>
      <div className="text-muted">{who}</div>
      <div className="mt-1 flex items-center gap-3">
        <code className="console bg-[#0f1013] px-2 py-1 text-ember select-all">{address}</code>
        <button
          className="btn px-2 py-1 text-xs"
          onClick={() => {
            void navigator.clipboard.writeText(address).then(() => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1200);
            });
          }}
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {children && <p className="mt-1.5 text-xs text-faint">{children}</p>}
    </li>
  );
}

function Overview({ inst }: { inst: Instance }) {
  const net = useQuery({ queryKey: ["network"], queryFn: api.network, staleTime: 60_000 });
  const facts: [string, string][] = [
    ["Address on this PC", `localhost:${inst.port}`],
    ["Port", String(inst.port)],
    ["Memory", formatMemory(inst.memory_mb)],
    ["Java", `Java ${inst.java_major}`],
    ["Process", inst.pid ? `PID ${inst.pid}` : "—"],
    ["Server ID", inst.id],
  ];
  return (
    <div className="grid max-w-4xl grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)]">
      <div className="block-card rise p-6">
        <h2 className="font-display mb-4 text-lg font-semibold">How friends join</h2>
        {inst.state === "running" ? (
          <ol className="space-y-4 text-sm">
            <JoinRow who="On this PC" address={`localhost:${inst.port}`} />
            {net.data?.lan_ipv4 ? (
              <JoinRow who="Friends on the same Wi-Fi / network" address={`${net.data.lan_ipv4}:${inst.port}`}>
                If they can't connect, Windows may be blocking Java: allow it when Windows Firewall asks, for private networks.
              </JoinRow>
            ) : (
              <li className="text-muted">Couldn't detect this PC's network address.</li>
            )}
            <li className="text-muted">Friends elsewhere on the internet: see "Invite friends over the internet" below.</li>
          </ol>
        ) : (
          <p className="text-sm text-muted">Start the server to see how people can join.</p>
        )}
        {inst.state === "unknown" && (
          <div className="mt-4">
            <Notice tone="warn">
              The manager restarted while this server was running and can't confirm whether it's still running. Check Task Manager for a
              "java" process before starting it again.
            </Notice>
          </div>
        )}
        {inst.state === "crashed" && <Diagnosis inst={inst} />}
      </div>
      <dl className="block-card rise grid grid-cols-[auto_1fr] gap-x-6 gap-y-3 p-6 text-sm" style={{ animationDelay: "60ms" }}>
        {facts.map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="text-muted">{k}</dt>
            <dd className="console truncate text-xs leading-5" title={v}>
              {v}
            </dd>
          </div>
        ))}
      </dl>
      <ConnectWizard inst={inst} />
      <ManageCard inst={inst} />
    </div>
  );
}

const MEMORY_STEPS = [1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384];

function ManageCard({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const [name, setName] = useState(inst.name);
  const [memIdx, setMemIdx] = useState(() => Math.max(0, MEMORY_STEPS.findIndex((m) => m >= inst.memory_mb)));
  const [msg, setMsg] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [typed, setTyped] = useState("");
  const stopped = inst.state === "stopped" || inst.state === "crashed" || inst.state === "install_failed";
  const memory = MEMORY_STEPS[memIdx] ?? inst.memory_mb;
  const dirty = name.trim() !== inst.name || memory !== inst.memory_mb;

  async function save() {
    setMsg(null);
    try {
      await call("instances.update", { instance: inst.id, name: name.trim(), memory_mb: memory });
      setMsg({ tone: "ok", text: "Saved." });
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } catch (e) {
      setMsg({ tone: "error", text: errorText(e) });
    }
  }

  async function remove() {
    try {
      await call("instances.delete", { instance: inst.id, confirm: true });
      setDeleting(false);
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } catch (e) {
      setDeleting(false);
      setMsg({ tone: "error", text: errorText(e) });
    }
  }

  return (
    <div className="block-card rise space-y-4 p-6 lg:col-span-2" style={{ animationDelay: "120ms" }}>
      <h2 className="font-display text-lg font-semibold">Manage</h2>
      {!stopped && <p className="text-xs text-muted">Stop the server to rename it, change its memory, or delete it.</p>}
      <div className="grid grid-cols-1 gap-5 md:grid-cols-2">
        <div>
          <label className="label" htmlFor="rename">Name</label>
          <input id="rename" className="field" maxLength={64} value={name} disabled={!stopped} onChange={(e) => setName(e.target.value)} />
        </div>
        <div>
          <label className="label" htmlFor="memory">Memory: {formatMemory(memory)}</label>
          <input
            id="memory"
            type="range"
            min={0}
            max={MEMORY_STEPS.length - 1}
            value={memIdx}
            disabled={!stopped}
            onChange={(e) => setMemIdx(Number(e.target.value))}
            className="mt-3 w-full accent-[var(--color-ember)]"
            aria-valuetext={formatMemory(memory)}
          />
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-3">
        <button className="btn btn-primary" disabled={!stopped || !dirty || !name.trim()} onClick={save}>
          Save changes
        </button>
        <button className="btn btn-danger ml-auto" disabled={!stopped} onClick={() => setDeleting(true)}>
          Delete server…
        </button>
      </div>
      {msg && <Notice tone={msg.tone}>{msg.text}</Notice>}
      {deleting && (
        <Confirm
          title={`Delete ${inst.name}?`}
          danger
          body={
            <>
              <p>The server disappears from the app. Its folder (worlds, backups, everything) is moved to the manager's trash folder, so it can still be recovered by hand.</p>
              <p>Type the server's name to confirm.</p>
            </>
          }
          confirmLabel="Delete"
          onClose={() => {
            setDeleting(false);
            setTyped("");
          }}
          onConfirm={() => typed === inst.name && remove()}
        >
          <input className="field" aria-label="Server name" value={typed} onChange={(e) => setTyped(e.target.value)} placeholder={inst.name} />
        </Confirm>
      )}
    </div>
  );
}
