import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Instance, type Software } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorDetail, errorText, formatMemory } from "../lib/text";
import { Field, Modal, Notice, Spinner } from "../components/ui";
import { PackFlow } from "./PackFlow";

type Step = "basics" | "review" | "installing" | "eula" | "error";

const SOFTWARE: { id: Software; name: string; blurb: string }[] = [
  { id: "vanilla", name: "Vanilla", blurb: "The official server, exactly like singleplayer." },
  { id: "paper", name: "Paper", blurb: "Faster, and supports plugins. Great default for friends." },
  { id: "fabric", name: "Fabric", blurb: "For Fabric mods. Friends need the same mods installed." },
];

const MEMORY_STEPS = [1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384];

export function CreateServer({ onClose, onCreated }: { onClose: () => void; onCreated: (id: string) => void }) {
  const qc = useQueryClient();
  const [source, setSource] = useState<"new" | "pack">("new");
  const [software, setSoftware] = useState<Software>("paper");
  const versions = useQuery({ queryKey: ["versions", software], queryFn: () => api.versions(software), staleTime: 10 * 60_000 });
  const existing = (qc.getQueryData<Instance[]>(["instances"]) ?? []).map((i) => i.port);
  const [name, setName] = useState("Friends SMP");
  const [version, setVersion] = useState<string>("");
  const [memoryIdx, setMemoryIdx] = useState(2);
  const [port, setPort] = useState(() => {
    let p = 25565;
    while (existing.includes(p)) p += 1;
    return p;
  });
  const [step, setStep] = useState<Step>("basics");
  const [task, setTask] = useState<string | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [phase, setPhase] = useState<"downloading" | "java">("downloading");
  const [created, setCreated] = useState<Instance | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [agreed, setAgreed] = useState(false);
  const [busy, setBusy] = useState(false);

  const offered = versions.data?.versions.map((v) => v.mc_version) ?? [];
  const chosenVersion = (offered.includes(version) ? version : "") || versions.data?.latest || "";
  const memory = MEMORY_STEPS[memoryIdx] ?? 2048;
  const nameOk = name.trim().length > 0 && name.trim().length <= 64;
  const portOk = port >= 1024 && port <= 65535;

  useTopic(task ? "tasks" : null, (e) => {
    const d = e.data as { task?: string; state?: string; phase?: string; progress?: { done: number; total: number | null }; result?: Instance; error?: unknown };
    if (!d || d.task !== task) return;
    if (d.phase === "java" && phase !== "java") {
      setPhase("java");
      setProgress(null);
    }
    if (d.progress?.total) setProgress(Math.round((d.progress.done / d.progress.total) * 100));
    if (d.state === "completed" && d.result) {
      setCreated(d.result);
      setStep("eula");
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } else if (d.state === "failed" || d.state === "cancelled") {
      setError(d.error);
      setStep("error");
    }
  }, null);

  async function create() {
    setBusy(true);
    setError(null);
    try {
      const { task } = await api.create({ software, name: name.trim(), mc_version: chosenVersion, port, memory_mb: memory });
      setTask(task);
      setStep("installing");
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  }

  async function acceptAndStart() {
    if (!created) return;
    setBusy(true);
    try {
      await api.acceptEula(created.id);
      await api.start(created.id);
      void qc.invalidateQueries({ queryKey: ["instances"] });
      onCreated(created.id);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title={step === "eula" ? "One last thing" : "Create a server"} onClose={step === "installing" ? () => {} : onClose} width={560}>
      {step === "basics" && (
        <div className="mb-5 grid grid-cols-2 gap-2" role="tablist" aria-label="How to create">
          {(["new", "pack"] as const).map((s) => (
            <button
              key={s}
              role="tab"
              aria-selected={source === s}
              onClick={() => setSource(s)}
              className={`border px-3 py-2 text-sm font-semibold ${source === s ? "border-ember bg-panel-2" : "border-line text-muted hover:text-ink"}`}
            >
              {s === "new" ? "New server" : "From a modpack"}
            </button>
          ))}
        </div>
      )}
      {step === "basics" && source === "pack" && <PackFlow onClose={onClose} onCreated={onCreated} />}
      {step === "basics" && source === "new" && (
        <div className="space-y-5">
          <Field label="Server name">
            {(id) => <input id={id} className="field" value={name} maxLength={64} onChange={(e) => setName(e.target.value)} />}
          </Field>
          <fieldset>
            <legend className="label">Server type</legend>
            <div className="grid grid-cols-3 gap-2">
              {SOFTWARE.map((sw) => (
                <label
                  key={sw.id}
                  className={`cursor-pointer border p-3 transition-colors ${
                    software === sw.id ? "border-ember bg-panel-2 shadow-[3px_3px_0_#08090b]" : "border-line hover:bg-panel-2/60"
                  }`}
                >
                  <input type="radio" name="software" className="sr-only" checked={software === sw.id} onChange={() => setSoftware(sw.id)} />
                  <span className="font-display block font-semibold">{sw.name}</span>
                  <span className="mt-1 block text-xs leading-snug text-muted">{sw.blurb}</span>
                </label>
              ))}
            </div>
          </fieldset>
          <Field label="Minecraft version" hint="Only versions we support for this server type are listed.">
            {(id) =>
              versions.isPending ? (
                <div className="py-2">
                  <Spinner label="Loading versions" />
                </div>
              ) : versions.isError ? (
                <Notice tone="error">{errorText(versions.error)}</Notice>
              ) : (
                <select id={id} className="field" value={chosenVersion} onChange={(e) => setVersion(e.target.value)}>
                  {versions.data.versions.map((v, i) => (
                    <option key={v.mc_version} value={v.mc_version}>
                      {v.mc_version}
                      {i === 0 ? " (latest)" : ""}
                    </option>
                  ))}
                </select>
              )
            }
          </Field>
          <Field label={`Memory: ${formatMemory(memory)}`} hint="2–4 GB suits a few friends. More helps with many players or big builds.">
            {(id) => (
              <input
                id={id}
                type="range"
                min={0}
                max={MEMORY_STEPS.length - 1}
                value={memoryIdx}
                onChange={(e) => setMemoryIdx(Number(e.target.value))}
                className="w-full accent-[var(--color-ember)]"
                aria-valuetext={formatMemory(memory)}
              />
            )}
          </Field>
          <Field label="Port" hint="25565 is the Minecraft default. Each server on this PC needs its own port.">
            {(id) => (
              <input id={id} className="field console w-40" type="number" min={1024} max={65535} value={port} onChange={(e) => setPort(Number(e.target.value))} />
            )}
          </Field>
          {existing.includes(port) && <Notice tone="warn">Another server already uses port {port}.</Notice>}
          <div className="flex justify-end gap-3 pt-2">
            <button className="btn" onClick={onClose}>
              Cancel
            </button>
            <button className="btn btn-primary" disabled={!nameOk || !portOk || !chosenVersion || existing.includes(port)} onClick={() => setStep("review")}>
              Review
            </button>
          </div>
        </div>
      )}

      {step === "review" && (
        <div className="space-y-4">
          <dl className="grid grid-cols-[140px_1fr] gap-y-2 text-sm">
            <dt className="text-muted">Name</dt>
            <dd>{name.trim()}</dd>
            <dt className="text-muted">Software</dt>
            <dd>
              {SOFTWARE.find((x) => x.id === software)?.name} for Minecraft {chosenVersion}
            </dd>
            <dt className="text-muted">Download from</dt>
            <dd className="console text-xs">
              {software === "paper"
                ? "fill.papermc.io · checked against PaperMC's SHA-256"
                : software === "fabric"
                  ? "meta.fabricmc.net (no published hash; fingerprint recorded) + Mojang server jar (hash checked)"
                  : "piston-data.mojang.com · checked against Mojang's published hash"}
            </dd>
            <dt className="text-muted">Memory</dt>
            <dd>{formatMemory(memory)}</dd>
            <dt className="text-muted">Port</dt>
            <dd className="console text-xs">{port}</dd>
          </dl>
          <Notice>
            Before the first start you'll be asked to accept the Minecraft End User License Agreement. Nothing is accepted on your behalf.
          </Notice>
          {error != null && <Notice tone="error">{errorText(error)}</Notice>}
          <div className="flex justify-between gap-3 pt-2">
            <button className="btn" onClick={() => setStep("basics")}>
              Back
            </button>
            <button className="btn btn-primary" disabled={busy} onClick={create}>
              {busy ? <Spinner /> : "Create server"}
            </button>
          </div>
        </div>
      )}

      {step === "installing" && (
        <div className="space-y-4" aria-live="polite">
          <p className="text-sm text-muted">
            {phase === "java"
              ? "Setting up Java for this server (Eclipse Temurin, downloaded from Adoptium and verified)…"
              : `Downloading and verifying ${SOFTWARE.find((x) => x.id === software)?.name} for Minecraft ${chosenVersion}…`}
          </p>
          <div className="h-3 border border-line bg-[#0f1013]" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={progress ?? undefined}>
            <div className="h-full bg-ember transition-all" style={{ width: `${progress ?? 4}%` }} />
          </div>
          <p className="console text-xs text-faint">{progress != null ? `${progress}%` : "starting"}</p>
        </div>
      )}

      {step === "eula" && created && (
        <div className="space-y-4">
          <p className="text-sm">
            <strong>{created.name}</strong> is installed. Minecraft servers require you to accept Mojang's End User License Agreement.
          </p>
          <p className="console rounded-none border border-line bg-[#0f1013] px-3 py-2 text-xs select-all">https://aka.ms/MinecraftEULA</p>
          <label className="flex cursor-pointer items-start gap-3 text-sm">
            <input type="checkbox" className="mt-0.5 h-4 w-4 accent-[var(--color-ember)]" checked={agreed} onChange={(e) => setAgreed(e.target.checked)} />
            <span>I have read and agree to the Minecraft EULA.</span>
          </label>
          {error != null && <Notice tone="error">{errorText(error)}</Notice>}
          <div className="flex justify-end gap-3 pt-2">
            <button className="btn" onClick={() => onCreated(created.id)}>
              Later
            </button>
            <button className="btn btn-primary" disabled={!agreed || busy} onClick={acceptAndStart}>
              {busy ? <Spinner /> : "Accept and start"}
            </button>
          </div>
        </div>
      )}

      {step === "error" && (
        <div className="space-y-4">
          <Notice tone="error">{errorText(error)}</Notice>
          {errorDetail(error) && <p className="console text-[11px] text-faint">{errorDetail(error)}</p>}
          <p className="text-sm text-muted">Nothing was left half-installed. You can try again.</p>
          <div className="flex justify-end gap-3">
            <button className="btn" onClick={onClose}>
              Close
            </button>
            <button className="btn btn-primary" onClick={() => setStep("review")}>
              Try again
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}
