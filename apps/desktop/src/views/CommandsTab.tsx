import { useEffect, useId, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type ActionDef, type ArgValue, type Instance, type Param } from "../lib/api";
import { errorText } from "../lib/text";
import { Modal, Notice, Spinner } from "../components/ui";

const LABELS: Record<string, string> = {
  time_day: "Make it day",
  time_noon: "Noon",
  time_night: "Make it night",
  weather_clear: "Clear weather",
  weather_rain: "Rain",
  weather_thunder: "Thunderstorm",
  difficulty: "Difficulty",
  keep_inventory: "Keep inventory on death",
  gamemode: "Game mode for a player",
  broadcast: "Broadcast a message",
  save_world: "Save the world now",
};
const ICONS: Record<string, string> = {
  time_day: "☀",
  time_noon: "◐",
  time_night: "☾",
  weather_clear: "◌",
  weather_rain: "☂",
  weather_thunder: "ϟ",
  difficulty: "⚔",
  keep_inventory: "⛁",
  gamemode: "◆",
  broadcast: "✉",
  save_world: "⛃",
};
const CATEGORY: Record<string, string> = { world: "World", rules: "Rules", players: "Players", chat: "Chat", server: "Server", macro: "Your macros" };

function defaults(params: Param[]): Record<string, ArgValue> {
  const out: Record<string, ArgValue> = {};
  for (const p of params) {
    if (p.kind === "bool") out[p.name] = true;
    else if (p.kind === "choice") out[p.name] = p.options[0] ?? "";
    else if (p.kind === "int") out[p.name] = p.min;
    else out[p.name] = "";
  }
  return out;
}

export function CommandsTab({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const catalog = useQuery({ queryKey: ["commands", inst.id], queryFn: () => api.commands(inst.id) });
  const players = useQuery({ queryKey: ["players", inst.id], queryFn: () => api.players(inst.id) });
  const [editing, setEditing] = useState<ActionDef | null>(null);
  const running = inst.state === "running";

  if (catalog.isPending) return <Spinner label="Loading commands" />;
  if (catalog.isError) return <Notice tone="error">{errorText(catalog.error)}</Notice>;
  const all = [...catalog.data.builtin, ...catalog.data.macros];
  const groups = [...new Set(all.map((a) => a.category))];
  const names = [...new Set([...(players.data?.online ?? []).map((p) => p.name), ...(players.data?.lists.known ?? []).map((p) => p.name)])];

  return (
    <div className="max-w-6xl space-y-8">
      {!running && <Notice>Start the server to run commands. You can still preview them and build macros.</Notice>}
      {groups.map((g) => (
        <section key={g} aria-labelledby={`cat-${g}`}>
          <h2 id={`cat-${g}`} className="label mb-3">
            {CATEGORY[g] ?? g}
          </h2>
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
            {all
              .filter((a) => a.category === g)
              .map((a, i) => (
                <ActionCard
                  key={a.id}
                  inst={inst}
                  action={a}
                  index={i}
                  running={running}
                  names={names}
                  onEdit={a.category === "macro" ? () => setEditing(a) : undefined}
                />
              ))}
          </div>
        </section>
      ))}
      <button className="btn" onClick={() => setEditing({ id: "", label: "", category: "macro", params: [], lines: [""] })}>
        + New macro
      </button>
      {editing && (
        <MacroEditor
          inst={inst}
          initial={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void qc.invalidateQueries({ queryKey: ["commands", inst.id] });
          }}
        />
      )}
    </div>
  );
}

function ActionCard({
  inst,
  action,
  index,
  running,
  names,
  onEdit,
}: {
  inst: Instance;
  action: ActionDef;
  index: number;
  running: boolean;
  names: string[];
  onEdit?: () => void;
}) {
  const [args, setArgs] = useState<Record<string, ArgValue>>(() => defaults(action.params));
  const [preview, setPreview] = useState<string[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [status, setStatus] = useState<"idle" | "busy" | "done">("idle");

  // Ask the daemon to render the exact console lines; it applies the same validation as Run.
  useEffect(() => {
    let live = true;
    const t = setTimeout(() => {
      api
        .preview(inst.id, action.id, args)
        .then((r) => live && (setPreview(r.lines), setProblem(null)))
        .catch((e) => live && (setPreview(null), setProblem(errorText(e))));
    }, 180);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [inst.id, action.id, args]);

  async function run() {
    setStatus("busy");
    try {
      await api.run(inst.id, action.id, args);
      setStatus("done");
      setTimeout(() => setStatus("idle"), 1600);
    } catch (e) {
      setProblem(errorText(e));
      setStatus("idle");
    }
  }

  const label = LABELS[action.id] ?? action.label;
  return (
    <div className="block-card rise flex flex-col gap-3 p-4" style={{ animationDelay: `${index * 35}ms` }}>
      <div className="flex items-start gap-3">
        <span aria-hidden className="grid h-9 w-9 shrink-0 place-items-center bg-panel-2 text-lg text-ember">
          {ICONS[action.id] ?? "▣"}
        </span>
        <div className="min-w-0 flex-1">
          <h3 className="font-semibold">{label}</h3>
          {onEdit && (
            <button className="text-xs text-muted underline-offset-2 hover:underline" onClick={onEdit}>
              Edit macro
            </button>
          )}
        </div>
      </div>
      {action.params.map((p) => (
        <ParamInput key={p.name} param={p} value={args[p.name]} names={names} onChange={(v) => setArgs((a) => ({ ...a, [p.name]: v }))} />
      ))}
      <div className="console min-h-[1.5rem] border-l-2 border-line pl-2 text-[11px] text-faint" aria-label="Command preview">
        {problem ? <span className="text-amber">{problem}</span> : preview?.map((l) => <div key={l}>/{l}</div>)}
      </div>
      <button className="btn btn-primary self-start" disabled={!running || !preview || status === "busy"} onClick={run}>
        {status === "busy" ? <Spinner /> : status === "done" ? "✓ Sent" : "Run"}
      </button>
    </div>
  );
}

function ParamInput({ param, value, names, onChange }: { param: Param; value: ArgValue | undefined; names: string[]; onChange: (v: ArgValue) => void }) {
  const id = `p-${param.name}-${useId().replace(/:/g, "")}`;
  const title = param.name.replace(/_/g, " ");
  if (param.kind === "bool") {
    return (
      <label className="flex items-center gap-2 text-sm">
        <input type="checkbox" className="h-4 w-4 accent-[var(--color-ember)]" checked={value === true} onChange={(e) => onChange(e.target.checked)} />
        {value ? "On" : "Off"}
      </label>
    );
  }
  return (
    <div>
      <label htmlFor={id} className="label">
        {title}
      </label>
      {param.kind === "choice" ? (
        <select id={id} className="field" value={String(value ?? "")} onChange={(e) => onChange(e.target.value)}>
          {param.options.map((o) => (
            <option key={o}>{o}</option>
          ))}
        </select>
      ) : param.kind === "int" ? (
        <input id={id} type="number" className="field console text-xs" min={param.min} max={param.max} value={Number(value ?? param.min)} onChange={(e) => onChange(Number(e.target.value))} />
      ) : (
        <>
          <input
            id={id}
            className="field text-sm"
            list={param.kind === "player" ? `${id}-names` : undefined}
            maxLength={param.kind === "text" ? param.max_len : param.kind === "player" ? 16 : 128}
            value={String(value ?? "")}
            onChange={(e) => onChange(e.target.value.replace(/[\r\n]/g, ""))}
          />
          {param.kind === "player" && (
            <datalist id={`${id}-names`}>
              {names.map((n) => (
                <option key={n} value={n} />
              ))}
            </datalist>
          )}
        </>
      )}
    </div>
  );
}

type ParamKind = "player" | "text" | "word" | "bool";

function MacroEditor({ inst, initial, onClose, onSaved }: { inst: Instance; initial: ActionDef; onClose: () => void; onSaved: () => void }) {
  const [name, setName] = useState(initial.label);
  const [lines, setLines] = useState(initial.lines.join("\n"));
  const [params, setParams] = useState<{ name: string; kind: ParamKind }[]>(
    initial.params.map((p) => ({ name: p.name, kind: (["player", "text", "word", "bool"].includes(p.kind) ? p.kind : "word") as ParamKind })),
  );
  const [scope, setScope] = useState<"server" | "all">("server");
  const [error, setError] = useState<string | null>(null);

  function toDef(): ActionDef {
    return {
      id: initial.id,
      label: name.trim(),
      category: "macro",
      lines: lines.split("\n").map((l) => l.trim()).filter(Boolean),
      params: params.map((p) => {
        const base = { name: p.name, label: p.name, optional: false };
        return p.kind === "text" ? { ...base, kind: "text" as const, max_len: 256 } : { ...base, kind: p.kind };
      }) as Param[],
    };
  }

  async function save() {
    setError(null);
    try {
      await api.saveMacro(scope === "server" ? inst.id : null, toDef());
      onSaved();
    } catch (e) {
      setError(errorText(e) + (e && typeof e === "object" && "detail" in e ? ` (${(e as { detail: string }).detail})` : ""));
    }
  }

  return (
    <Modal title={initial.id ? "Edit macro" : "New macro"} onClose={onClose} width={620}>
      <div className="space-y-4">
        <div>
          <label className="label" htmlFor="macro-name">
            Name
          </label>
          <input id="macro-name" className="field" maxLength={64} value={name} onChange={(e) => setName(e.target.value)} placeholder="Start the event" />
        </div>
        <div>
          <label className="label" htmlFor="macro-lines">
            Commands, one per line
          </label>
          <textarea
            id="macro-lines"
            className="field console min-h-32 text-xs"
            value={lines}
            onChange={(e) => setLines(e.target.value)}
            placeholder={"time set day\nweather clear\ntp {player} 0 100 0"}
          />
          <p className="mt-1.5 text-xs text-muted">
            Use <code className="console text-ember">{"{name}"}</code> for a value you fill in when running it. The command itself (first word) must be written out.
          </p>
        </div>
        <div>
          <span className="label">Fill-in values</span>
          <div className="space-y-2">
            {params.map((p, i) => (
              <div key={i} className="flex gap-2">
                <input
                  aria-label="Value name"
                  className="field console text-xs"
                  value={p.name}
                  onChange={(e) => setParams((ps) => ps.map((q, j) => (j === i ? { ...q, name: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, "") } : q)))}
                />
                <select
                  aria-label="Value type"
                  className="field w-40"
                  value={p.kind}
                  onChange={(e) => setParams((ps) => ps.map((q, j) => (j === i ? { ...q, kind: e.target.value as ParamKind } : q)))}
                >
                  <option value="player">Player</option>
                  <option value="word">Single word</option>
                  <option value="text">Text (last on its line)</option>
                  <option value="bool">On/Off</option>
                </select>
                <button className="btn btn-ghost" aria-label="Remove value" onClick={() => setParams((ps) => ps.filter((_, j) => j !== i))}>
                  ✕
                </button>
              </div>
            ))}
            <button className="btn px-2.5 py-1 text-xs" onClick={() => setParams((ps) => [...ps, { name: "player", kind: "player" }])}>
              + Add value
            </button>
          </div>
        </div>
        <fieldset className="flex gap-4 text-sm">
          <legend className="label">Available on</legend>
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "server"} onChange={() => setScope("server")} className="accent-[var(--color-ember)]" /> This server
          </label>
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "all"} onChange={() => setScope("all")} className="accent-[var(--color-ember)]" /> All servers
          </label>
        </fieldset>
        {error && <Notice tone="error">{error}</Notice>}
        <div className="flex justify-between pt-2">
          {initial.id ? (
            <button
              className="btn btn-danger"
              onClick={async () => {
                await api.deleteMacro(initial.id).catch(() => undefined);
                onSaved();
              }}
            >
              Delete
            </button>
          ) : (
            <span />
          )}
          <div className="flex gap-3">
            <button className="btn" onClick={onClose}>
              Cancel
            </button>
            <button className="btn btn-primary" disabled={!name.trim() || !lines.trim()} onClick={save}>
              Save macro
            </button>
          </div>
        </div>
      </div>
    </Modal>
  );
}
