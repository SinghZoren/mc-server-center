import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { call, type Instance } from "../lib/api";
import { errorText } from "../lib/text";
import { Notice, Spinner } from "../components/ui";

type Kind = { kind: "bool" } | { kind: "int"; min: number; max: number } | { kind: "choice"; options: string[] } | { kind: "text"; max_len: number };
type SettingDef = { key: string; group: string; label: string; help: string; default: string; warning: [string, string] | null } & Kind;

export function SettingsTab({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["config", inst.id];
  const data = useQuery({
    queryKey: key,
    queryFn: () => call<{ schema: SettingDef[]; values: Record<string, string>; running: boolean }>("config.get", { instance: inst.id }),
  });
  const [draft, setDraft] = useState<Record<string, string>>({});
  const [result, setResult] = useState<{ tone: "ok" | "warn" | "error"; text: string } | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (data.data) setDraft(data.data.values);
  }, [data.data]);

  const dirty = useMemo(
    () => (data.data ? Object.keys(draft).filter((k) => draft[k] !== data.data.values[k]) : []),
    [draft, data.data],
  );

  if (data.isPending) return <Spinner label="Loading settings" />;
  if (data.isError) return <Notice tone="error">{errorText(data.error)}</Notice>;
  const groups = [...new Set(data.data.schema.map((d) => d.group))];

  async function save() {
    setSaving(true);
    setResult(null);
    try {
      const values = Object.fromEntries(dirty.map((k) => [k, draft[k] ?? ""]));
      const r = await call<{ changed: string[]; restart_required: boolean }>("config.set_typed", { instance: inst.id, values });
      setResult(
        r.restart_required
          ? { tone: "warn", text: "Saved. Restart the server for the changes to take effect." }
          : { tone: "ok", text: r.changed.length ? "Saved. They'll apply the next time the server starts." : "Nothing changed." },
      );
      void qc.invalidateQueries({ queryKey: key });
    } catch (e) {
      setResult({ tone: "error", text: errorText(e) + (e && typeof e === "object" && "detail" in e ? ` — ${(e as { detail: string }).detail}` : "") });
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="max-w-3xl space-y-8 pb-24">
      {groups.map((g) => (
        <section key={g} aria-labelledby={`g-${g}`}>
          <h2 id={`g-${g}`} className="label mb-3">
            {g}
          </h2>
          <div className="block-card divide-y divide-line">
            {data.data.schema
              .filter((d) => d.group === g)
              .map((d) => (
                <SettingRow key={d.key} def={d} value={draft[d.key] ?? d.default} onChange={(v) => setDraft((x) => ({ ...x, [d.key]: v }))} />
              ))}
          </div>
        </section>
      ))}
      <div className="fixed right-10 bottom-6 flex items-center gap-4">
        {result && (
          <div className="max-w-md">
            <Notice tone={result.tone}>{result.text}</Notice>
          </div>
        )}
        <button className="btn" disabled={!dirty.length || saving} onClick={() => data.data && setDraft(data.data.values)}>
          Undo changes
        </button>
        <button className="btn btn-primary" disabled={!dirty.length || saving} onClick={save}>
          {saving ? <Spinner /> : `Save${dirty.length ? ` (${dirty.length})` : ""}`}
        </button>
      </div>
    </div>
  );
}

function SettingRow({ def, value, onChange }: { def: SettingDef; value: string; onChange: (v: string) => void }) {
  const id = `set-${def.key}`;
  const warn = def.warning && value === def.warning[0] ? def.warning[1] : null;
  return (
    <div className="flex flex-wrap items-start justify-between gap-4 px-5 py-4">
      <div className="min-w-0 flex-1 basis-64">
        <label htmlFor={id} className="font-semibold">
          {def.label}
        </label>
        <p className="mt-0.5 text-xs text-muted">{def.help}</p>
        {warn && <p className="mt-1.5 text-xs text-amber">{warn}</p>}
      </div>
      <div className="w-56">
        {def.kind === "bool" ? (
          <button
            id={id}
            role="switch"
            aria-checked={value === "true"}
            onClick={() => onChange(value === "true" ? "false" : "true")}
            className={`btn w-24 justify-center ${value === "true" ? "btn-primary" : ""}`}
          >
            {value === "true" ? "On" : "Off"}
          </button>
        ) : def.kind === "choice" ? (
          <select id={id} className="field" value={value} onChange={(e) => onChange(e.target.value)}>
            {def.options.map((o) => (
              <option key={o} value={o}>
                {o[0]?.toUpperCase()}
                {o.slice(1)}
              </option>
            ))}
          </select>
        ) : def.kind === "int" ? (
          <input id={id} type="number" className="field console text-xs" min={def.min} max={def.max} value={value} onChange={(e) => onChange(e.target.value)} />
        ) : (
          <input id={id} className="field" maxLength={def.max_len} value={value} onChange={(e) => onChange(e.target.value.replace(/[\r\n]/g, ""))} />
        )}
      </div>
    </div>
  );
}
