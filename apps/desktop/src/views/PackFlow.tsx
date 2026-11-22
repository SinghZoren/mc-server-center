import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, call, type Instance } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorDetail, errorText, formatMemory } from "../lib/text";
import { Notice, Spinner } from "../components/ui";

interface PackHit {
  project_id: string;
  slug: string;
  title: string;
  description: string;
  author: string;
  downloads: number;
  environment: string[];
}

const MEMORY_STEPS = [2048, 3072, 4096, 6144, 8192, 12288, 16384];
const PHASES: Record<string, string> = {
  pack: "Downloading and checking the modpack…",
  server: "Setting up the Fabric server it needs…",
  files: "Downloading and verifying the pack's mods…",
  overrides: "Applying the pack's settings…",
  java: "Setting up Java…",
};

const serverFriendly = (env: string[]) => env.some((e) => e !== "client_only" && e !== "unknown");

/** Create a server from a Modrinth modpack (PACK-01..04). */
export function PackFlow({ onClose, onCreated }: { onClose: () => void; onCreated: (id: string) => void }) {
  const qc = useQueryClient();
  const existing = (qc.getQueryData<Instance[]>(["instances"]) ?? []).map((i) => i.port);
  const [query, setQuery] = useState("");
  const [debounced, setDebounced] = useState("");
  useEffect(() => {
    const t = setTimeout(() => setDebounced(query.trim()), 300);
    return () => clearTimeout(t);
  }, [query]);
  const results = useQuery({
    queryKey: ["packs", debounced],
    queryFn: () => call<{ hits: PackHit[] }>("modpacks.search", { query: debounced }),
    staleTime: 60_000,
  });
  const [pick, setPick] = useState<PackHit | null>(null);
  const [name, setName] = useState("");
  const [memIdx, setMemIdx] = useState(2);
  const [port, setPort] = useState(() => {
    let p = 25565;
    while (existing.includes(p)) p += 1;
    return p;
  });
  const [task, setTask] = useState<string | null>(null);
  const [phase, setPhase] = useState("pack");
  const [fileProgress, setFileProgress] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [done, setDone] = useState<{ result: Instance; pack: { name: string; version: string; files: number; skipped_client_only: number } } | null>(null);
  const [agreed, setAgreed] = useState(false);
  const [busy, setBusy] = useState(false);

  useTopic(task ? "tasks" : null, (e) => {
    const d = e.data as { task?: string; state?: string; phase?: string; progress?: { done: number; total: number }; result?: typeof done; error?: unknown };
    if (d?.task !== task) return;
    if (d.phase) setPhase(d.phase);
    if (d.phase === "files" && d.progress) setFileProgress(`${d.progress.done} of ${d.progress.total}`);
    if (d.state === "completed" && d.result) {
      setTask(null);
      setDone(d.result);
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } else if (d.state === "failed") {
      setTask(null);
      setError(d.error);
    }
  }, null);

  const memory = MEMORY_STEPS[memIdx] ?? 4096;

  async function install() {
    if (!pick) return;
    setError(null);
    try {
      const r = await call<{ task: string }>("modpacks.install", { project: pick.project_id, name: name.trim() || pick.title, port, memory_mb: memory });
      setPhase("pack");
      setTask(r.task);
    } catch (e) {
      setError(e);
    }
  }

  if (done) {
    return (
      <div className="space-y-4">
        <Notice tone="ok">
          <strong>{done.result.name}</strong> is ready: {done.pack.files} files from {done.pack.name} were installed and verified
          {done.pack.skipped_client_only ? `, and ${done.pack.skipped_client_only} client-only files were left out` : ""}.
        </Notice>
        <div className="block-card p-4 text-sm">
          <p className="font-semibold">Tell your friends</p>
          <p className="mt-1 text-muted">
            To join, players need the same modpack in their game: install <strong className="text-ink">{done.pack.name}</strong> version{" "}
            <span className="console text-xs text-ink">{done.pack.version}</span> from Modrinth (for example with the Modrinth App or Prism Launcher).
          </p>
        </div>
        <p className="console border border-line bg-[#0f1013] px-3 py-2 text-xs select-all">https://aka.ms/MinecraftEULA</p>
        <label className="flex cursor-pointer items-start gap-3 text-sm">
          <input type="checkbox" className="mt-0.5 h-4 w-4 accent-[var(--color-ember)]" checked={agreed} onChange={(e) => setAgreed(e.target.checked)} />
          <span>I have read and agree to the Minecraft EULA.</span>
        </label>
        {error != null && <Notice tone="error">{errorText(error)}</Notice>}
        <div className="flex justify-end gap-3">
          <button className="btn" onClick={() => onCreated(done.result.id)}>Later</button>
          <button
            className="btn btn-primary"
            disabled={!agreed || busy}
            onClick={async () => {
              setBusy(true);
              try {
                await api.acceptEula(done.result.id);
                await api.start(done.result.id);
                onCreated(done.result.id);
              } catch (e) {
                setError(e);
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? <Spinner /> : "Accept and start"}
          </button>
        </div>
      </div>
    );
  }

  if (task) {
    return (
      <div className="space-y-3" aria-live="polite">
        <p className="text-sm text-muted">{PHASES[phase] ?? "Working…"}</p>
        {phase === "files" && fileProgress && <p className="console text-xs text-faint">{fileProgress}</p>}
        <Spinner />
      </div>
    );
  }

  if (pick) {
    return (
      <div className="space-y-5">
        <div className="block-card p-4">
          <div className="font-semibold">{pick.title}</div>
          <p className="mt-1 text-sm text-muted">{pick.description}</p>
          {!serverFriendly(pick.environment) && (
            <p className="mt-2 text-xs text-amber">This pack is mostly made for players' games; only its server-side mods will be installed.</p>
          )}
        </div>
        <div>
          <label className="label" htmlFor="pack-name">Server name</label>
          <input id="pack-name" className="field" maxLength={64} value={name} placeholder={pick.title} onChange={(e) => setName(e.target.value)} />
        </div>
        <div>
          <label className="label" htmlFor="pack-mem">Memory: {formatMemory(memory)}</label>
          <input id="pack-mem" type="range" min={0} max={MEMORY_STEPS.length - 1} value={memIdx} onChange={(e) => setMemIdx(Number(e.target.value))} className="w-full accent-[var(--color-ember)]" aria-valuetext={formatMemory(memory)} />
          <p className="mt-1.5 text-xs text-muted">Modpacks usually need 4–8 GB.</p>
        </div>
        <div>
          <label className="label" htmlFor="pack-port">Port</label>
          <input id="pack-port" className="field console w-40" type="number" min={1024} max={65535} value={port} onChange={(e) => setPort(Number(e.target.value))} />
        </div>
        <Notice>Everything is checked before the server is created. Mods are third-party code; install packs you trust.</Notice>
        {error != null && (
          <Notice tone="error">
            {errorText(error)}
            {errorDetail(error) && <span className="console mt-1 block text-[11px] text-faint">{errorDetail(error)}</span>}
          </Notice>
        )}
        <div className="flex justify-between gap-3">
          <button className="btn" onClick={() => setPick(null)}>Back</button>
          <button className="btn btn-primary" disabled={existing.includes(port) || port < 1024} onClick={install}>Create server</button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <input className="field" placeholder="Search Fabric modpacks on Modrinth…" aria-label="Search modpacks" value={query} maxLength={100} onChange={(e) => setQuery(e.target.value)} />
      {results.isPending ? (
        <Spinner label="Searching" />
      ) : results.isError ? (
        <Notice tone="error">{errorText(results.error)}</Notice>
      ) : (
        <ul className="max-h-[50vh] space-y-2 overflow-y-auto pr-1">
          {results.data.hits.map((h) => (
            <li key={h.project_id}>
              <button
                className="block w-full border border-line px-3 py-2.5 text-left transition-colors hover:border-ember/60 hover:bg-panel-2"
                onClick={() => {
                  setPick(h);
                  setName(h.title.slice(0, 64));
                }}
              >
                <span className="flex items-baseline justify-between gap-3">
                  <span className="truncate font-semibold">{h.title}</span>
                  <span className="console shrink-0 text-[10px] text-faint">{h.downloads >= 1e6 ? `${(h.downloads / 1e6).toFixed(1)}M` : Math.round(h.downloads / 1e3) + "k"}</span>
                </span>
                <span className="mt-0.5 line-clamp-1 block text-xs text-muted">{h.description}</span>
                {!serverFriendly(h.environment) && <span className="mt-1 block text-[11px] text-amber">Mostly for players' games</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex justify-end">
        <button className="btn" onClick={onClose}>Cancel</button>
      </div>
    </div>
  );
}
