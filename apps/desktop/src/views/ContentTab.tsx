import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { call, type Instance } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorText } from "../lib/text";
import { Confirm, Modal, Notice, Spinner } from "../components/ui";

interface Hit {
  project_id: string;
  slug: string;
  title: string;
  description: string;
  author: string;
  downloads: number;
  license: string | null;
}
interface PlanItem {
  project_id: string;
  title: string;
  version_number: string;
  version_type: string;
  filename: string;
  size: number;
  required_by: string | null;
}
interface Plan {
  install: PlanItem[];
  already_installed: string[];
  optional: string[];
  conflicts: string[];
  warnings: string[];
}
interface Installed {
  project_id: string;
  title: string;
  version_number: string;
  file: string;
  required_by: string[];
}

const compact = (n: number) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n));

export function ContentTab({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const noun = inst.software === "paper" ? "plugin" : "mod";
  const installedKey = ["content", inst.id];
  const installed = useQuery({
    queryKey: installedKey,
    queryFn: () => call<{ supported: boolean; items: Installed[] }>("content.list", { instance: inst.id }),
  });
  const [query, setQuery] = useState("");
  const [debounced, setDebounced] = useState("");
  useEffect(() => {
    const t = setTimeout(() => setDebounced(query.trim()), 300);
    return () => clearTimeout(t);
  }, [query]);
  const supported = installed.data?.supported ?? false;
  const results = useQuery({
    queryKey: ["content-search", inst.id, debounced],
    queryFn: () => call<{ hits: Hit[]; total_hits: number }>("content.search", { instance: inst.id, query: debounced }),
    enabled: supported,
    staleTime: 60_000,
  });
  const [planning, setPlanning] = useState<Hit | null>(null);
  const [removing, setRemoving] = useState<Installed | null>(null);
  const [task, setTask] = useState<string | null>(null);
  const [message, setMessage] = useState<{ tone: "ok" | "warn" | "error"; text: string } | null>(null);

  useTopic(task ? "tasks" : null, (e) => {
    const d = e.data as { task?: string; state?: string; error?: unknown; result?: { installed: { title: string }[]; restart_required: boolean } };
    if (d?.task !== task) return;
    if (d.state === "completed") {
      setTask(null);
      const names = d.result?.installed.map((i) => i.title).join(", ");
      setMessage({
        tone: d.result?.restart_required ? "warn" : "ok",
        text: `Installed ${names || "nothing new"}.${d.result?.restart_required ? " Restart the server to load it." : ""} A safety snapshot was saved in Backups first.`,
      });
      void qc.invalidateQueries({ queryKey: installedKey });
    } else if (d.state === "failed") {
      setTask(null);
      setMessage({ tone: "error", text: errorText(d.error) });
    }
  }, null);

  if (installed.isPending) return <Spinner />;
  if (installed.isError) return <Notice tone="error">{errorText(installed.error)}</Notice>;
  if (!supported) {
    return (
      <div className="max-w-xl">
        <Notice>
          Vanilla servers can't use mods or plugins. Create a <strong>Fabric</strong> server for mods or a <strong>Paper</strong> server for plugins.
        </Notice>
      </div>
    );
  }

  const installedIds = new Set(installed.data.items.map((i) => i.project_id));

  return (
    <div className="grid max-w-6xl grid-cols-1 gap-8 xl:grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)]">
      <section aria-labelledby="find-h" className="min-w-0 space-y-4">
        <h2 id="find-h" className="font-display text-xl font-semibold">
          Find {noun}s <span className="text-sm font-normal text-muted">on Modrinth, for Minecraft {inst.mc_version}</span>
        </h2>
        <input
          className="field"
          placeholder={`Search ${noun}s…`}
          aria-label={`Search ${noun}s`}
          value={query}
          maxLength={100}
          onChange={(e) => setQuery(e.target.value)}
        />
        <p className="text-xs text-faint">
          {noun === "mod" ? "Mods" : "Plugins"} run code on your server. Install ones from creators you trust; popular projects are a good sign.
        </p>
        {message && <Notice tone={message.tone}>{message.text}</Notice>}
        {task && (
          <p className="text-sm text-muted" aria-live="polite">
            <Spinner /> Installing…
          </p>
        )}
        {results.isPending ? (
          <Spinner label="Searching" />
        ) : results.isError ? (
          <Notice tone="error">{errorText(results.error)}</Notice>
        ) : (
          <ul className="space-y-3">
            {results.data.hits.map((h, i) => (
              <li key={h.project_id} className="block-card rise flex items-start gap-4 p-4" style={{ animationDelay: `${i * 25}ms` }}>
                <div className="min-w-0 flex-1">
                  <div className="flex items-baseline gap-2">
                    <h3 className="truncate font-semibold">{h.title}</h3>
                    <span className="shrink-0 text-xs text-faint">by {h.author}</span>
                  </div>
                  <p className="mt-1 line-clamp-2 text-sm text-muted">{h.description}</p>
                  <p className="console mt-2 text-[10px] text-faint">
                    {compact(h.downloads)} downloads{h.license ? ` · ${h.license}` : ""}
                  </p>
                </div>
                {installedIds.has(h.project_id) ? (
                  <span className="text-xs text-moss">✓ Installed</span>
                ) : (
                  <button className="btn shrink-0 px-3 py-1.5 text-xs" disabled={!!task} onClick={() => setPlanning(h)}>
                    Add…
                  </button>
                )}
              </li>
            ))}
            {results.data.hits.length === 0 && <p className="text-sm text-muted">Nothing found for this version.</p>}
          </ul>
        )}
      </section>

      <section aria-labelledby="inst-h" className="min-w-0">
        <h2 id="inst-h" className="font-display mb-4 text-xl font-semibold">
          Installed <span className="text-muted">{installed.data.items.length}</span>
        </h2>
        <ul className="block-card divide-y divide-line">
          {installed.data.items.length === 0 && <li className="p-4 text-sm text-faint">No {noun}s yet.</li>}
          {installed.data.items.map((i) => (
            <li key={i.project_id} className="flex items-center gap-3 px-4 py-3">
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm font-semibold">{i.title}</div>
                <div className="console truncate text-[10px] text-faint">
                  {i.version_number}
                  {i.required_by.length ? ` · needed by ${i.required_by.join(", ")}` : ""}
                </div>
              </div>
              <button className="btn btn-ghost px-2 py-1 text-xs text-muted hover:text-ink" onClick={() => setRemoving(i)}>
                Remove
              </button>
            </li>
          ))}
        </ul>
      </section>

      {planning && (
        <PlanDialog
          inst={inst}
          hit={planning}
          onClose={() => setPlanning(null)}
          onInstall={async () => {
            setPlanning(null);
            setMessage(null);
            try {
              const r = await call<{ task: string }>("content.install", { instance: inst.id, project: planning.project_id });
              setTask(r.task);
            } catch (e) {
              setMessage({ tone: "error", text: errorText(e) });
            }
          }}
        />
      )}
      {removing && (
        <Confirm
          title={`Remove ${removing.title}?`}
          body={
            <>
              <p>The file is deleted from the server. A safety snapshot is saved in Backups first, so you can undo this.</p>
              {installed.data.items.some((x) => removing.required_by.includes(x.title)) && (
                <p className="text-amber">Other {noun}s installed it as a requirement and may stop working.</p>
              )}
            </>
          }
          confirmLabel="Remove"
          danger
          onClose={() => setRemoving(null)}
          onConfirm={async () => {
            const target = removing;
            setRemoving(null);
            try {
              const r = await call<{ may_break: string[]; restart_required: boolean }>("content.remove", { instance: inst.id, project: target.project_id });
              setMessage({
                tone: r.may_break.length || r.restart_required ? "warn" : "ok",
                text: `Removed ${target.title}.${r.may_break.length ? ` ${r.may_break.join(", ")} may need it.` : ""}${r.restart_required ? " Restart the server to apply." : ""}`,
              });
              void qc.invalidateQueries({ queryKey: installedKey });
            } catch (e) {
              setMessage({ tone: "error", text: errorText(e) });
            }
          }}
        />
      )}
    </div>
  );
}

function PlanDialog({ inst, hit, onClose, onInstall }: { inst: Instance; hit: Hit; onClose: () => void; onInstall: () => void }) {
  const plan = useQuery({
    queryKey: ["content-plan", inst.id, hit.project_id],
    queryFn: () => call<Plan>("content.plan", { instance: inst.id, project: hit.project_id }),
    retry: false,
  });
  const kb = (n: number) => (n > 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);
  return (
    <Modal title={`Add ${hit.title}`} onClose={onClose} width={600}>
      {plan.isPending ? (
        <Spinner label="Checking compatibility" />
      ) : plan.isError ? (
        <div className="space-y-4">
          <Notice tone="error">{errorText(plan.error)}{plan.error && typeof plan.error === "object" && "detail" in plan.error ? ` — ${(plan.error as { detail: string }).detail}` : ""}</Notice>
          <div className="flex justify-end">
            <button className="btn" onClick={onClose}>Close</button>
          </div>
        </div>
      ) : (
        <div className="space-y-4">
          <p className="text-sm text-muted">These files will be added (verified against Modrinth's SHA-512 hashes):</p>
          <ul className="space-y-2">
            {plan.data.install.map((i) => (
              <li key={i.project_id} className="flex items-baseline justify-between gap-3 border-l-2 border-ember/60 pl-3 text-sm">
                <span>
                  <strong>{i.title}</strong> <span className="console text-[11px] text-faint">{i.version_number}</span>
                  {i.version_type !== "release" && <span className="ml-2 text-xs text-amber">{i.version_type}</span>}
                  {i.required_by && <span className="block text-xs text-muted">needed by {i.required_by}</span>}
                </span>
                <span className="console shrink-0 text-[11px] text-faint">{kb(i.size)}</span>
              </li>
            ))}
          </ul>
          {plan.data.already_installed.length > 0 && <p className="text-xs text-muted">Already installed requirements are reused.</p>}
          {plan.data.warnings.map((w) => (
            <Notice key={w} tone="warn">{w}</Notice>
          ))}
          {plan.data.conflicts.map((c) => (
            <Notice key={c} tone="error">{c}</Notice>
          ))}
          {plan.data.optional.length > 0 && (
            <p className="text-xs text-faint">It also works with {plan.data.optional.length} optional add-on(s) you can install separately.</p>
          )}
          <p className="text-xs text-faint">
            {inst.software === "paper"
              ? "Plugins run only on the server; players don't need to install anything unless the plugin's page says so."
              : "Players usually need the same mods (and Fabric) installed in their game to join."}
          </p>
          <div className="flex justify-end gap-3 pt-2">
            <button className="btn" onClick={onClose}>Cancel</button>
            <button className="btn btn-primary" disabled={plan.data.conflicts.length > 0 || plan.data.install.length === 0} onClick={onInstall}>
              Install {plan.data.install.length > 1 ? `${plan.data.install.length} files` : ""}
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}
