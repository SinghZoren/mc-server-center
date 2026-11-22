import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { call, type Instance } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorText } from "../lib/text";
import { Confirm, Notice, Spinner } from "../components/ui";

interface BackupInfo {
  id: string;
  created_at_ms: number;
  reason: string;
  scope: "full" | "world";
  quiesced: boolean;
  total_bytes: number;
  files: unknown[];
  mc_version: string;
}

function bytes(n: number): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)} GB`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(n / 1024))} KB`;
}

export function BackupsTab({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["backups", inst.id];
  const list = useQuery({
    queryKey: key,
    queryFn: () => call<{ backups: BackupInfo[]; estimate_full_bytes: number | null }>("backups.list", { instance: inst.id }),
  });
  const [task, setTask] = useState<string | null>(null);
  const [message, setMessage] = useState<{ tone: "ok" | "error" | "warn"; text: string } | null>(null);
  const [verifying, setVerifying] = useState<string | null>(null);
  const [verified, setVerified] = useState<Record<string, boolean>>({});
  const [restoring, setRestoring] = useState<BackupInfo | null>(null);
  const [deleting, setDeleting] = useState<BackupInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const stopped = inst.state === "stopped" || inst.state === "crashed";

  useTopic(task ? "tasks" : null, (e) => {
    const d = e.data as { task?: string; state?: string; result?: { quiesced: boolean; bytes: number }; error?: unknown };
    if (d?.task !== task) return;
    if (d.state === "completed") {
      setTask(null);
      const q = d.result?.quiesced;
      setMessage({
        tone: q === false && inst.state === "running" ? "warn" : "ok",
        text:
          q === false && inst.state === "running"
            ? "Backup saved, but the server didn't confirm its save in time. It's probably fine; stop the server for a guaranteed-consistent backup."
            : `Backup saved (${bytes(d.result?.bytes ?? 0)}).`,
      });
      void qc.invalidateQueries({ queryKey: key });
    } else if (d.state === "failed") {
      setTask(null);
      setMessage({ tone: "error", text: errorText(d.error) });
    }
  }, null);

  async function create(scope: "full" | "world") {
    setMessage(null);
    try {
      const r = await call<{ task: string }>("backups.create", { instance: inst.id, scope });
      setTask(r.task);
    } catch (e) {
      setMessage({ tone: "error", text: errorText(e) });
    }
  }

  async function verify(b: BackupInfo) {
    setVerifying(b.id);
    try {
      const r = await call<{ ok: boolean; problems: string[] }>("backups.verify", { instance: inst.id, backup: b.id });
      setVerified((v) => ({ ...v, [b.id]: r.ok }));
      if (!r.ok) setMessage({ tone: "error", text: `This backup is damaged: ${r.problems.slice(0, 3).join("; ")}` });
    } catch (e) {
      setMessage({ tone: "error", text: errorText(e) });
    } finally {
      setVerifying(null);
    }
  }

  async function restore(b: BackupInfo) {
    setBusy(true);
    setMessage(null);
    try {
      const r = await call<{ previous_kept_at: string }>("backups.restore", { instance: inst.id, backup: b.id, confirm: true });
      setMessage({ tone: "ok", text: `Restored. Your previous files were kept in ${r.previous_kept_at}` });
    } catch (e) {
      setMessage({ tone: "error", text: errorText(e) });
    } finally {
      setBusy(false);
      setRestoring(null);
    }
  }

  async function remove(b: BackupInfo) {
    setBusy(true);
    try {
      await call("backups.delete", { instance: inst.id, backup: b.id });
      void qc.invalidateQueries({ queryKey: key });
    } catch (e) {
      setMessage({ tone: "error", text: errorText(e) });
    } finally {
      setBusy(false);
      setDeleting(null);
    }
  }

  return (
    <div className="max-w-4xl space-y-6">
      <div className="block-card flex flex-wrap items-center justify-between gap-4 p-5">
        <div>
          <h2 className="font-display text-lg font-semibold">Back up this server</h2>
          <p className="text-sm text-muted">
            Saved inside the server's folder and checked file by file.
            {list.data?.estimate_full_bytes ? ` A full backup is about ${bytes(list.data.estimate_full_bytes)} before compression.` : ""}
          </p>
        </div>
        <div className="flex gap-3">
          <button className="btn" disabled={!!task} onClick={() => create("world")}>
            Worlds only
          </button>
          <button className="btn btn-primary" disabled={!!task} onClick={() => create("full")}>
            {task ? <Spinner label="Backing up" /> : "Full backup"}
          </button>
        </div>
      </div>
      {message && <Notice tone={message.tone}>{message.text}</Notice>}

      {list.isPending ? (
        <Spinner />
      ) : list.isError ? (
        <Notice tone="error">{errorText(list.error)}</Notice>
      ) : list.data.backups.length === 0 ? (
        <p className="text-sm text-muted">No backups yet. Take one before trying new mods, plugins or big changes.</p>
      ) : (
        <ul className="space-y-3">
          {list.data.backups.map((b, i) => (
            <li key={b.id} className="block-card rise flex flex-wrap items-center gap-4 p-4" style={{ animationDelay: `${i * 30}ms` }}>
              <div className="min-w-0 flex-1">
                <div className="font-semibold">{new Date(b.created_at_ms).toLocaleString()}</div>
                <div className="text-xs text-muted">
                  {b.scope === "full" ? "Full" : "Worlds only"} · {bytes(b.total_bytes)} · {b.files.length} files · Minecraft {b.mc_version}
                  {b.reason !== "manual" ? ` · ${b.reason}` : ""}
                  {verified[b.id] === true && <span className="ml-2 text-moss">✓ verified</span>}
                  {verified[b.id] === false && <span className="ml-2 text-danger">damaged</span>}
                </div>
              </div>
              <button className="btn px-2.5 py-1.5 text-xs" disabled={verifying === b.id} onClick={() => verify(b)}>
                {verifying === b.id ? <Spinner /> : "Check"}
              </button>
              <button
                className="btn px-2.5 py-1.5 text-xs"
                disabled={!stopped || busy}
                title={stopped ? undefined : "Stop the server first"}
                onClick={() => setRestoring(b)}
              >
                Restore…
              </button>
              <button className="btn btn-ghost px-2 py-1 text-xs text-muted" disabled={busy} onClick={() => setDeleting(b)}>
                Delete
              </button>
            </li>
          ))}
        </ul>
      )}
      {!stopped && list.data && list.data.backups.length > 0 && <p className="text-xs text-faint">Stop the server to restore a backup.</p>}

      {restoring && (
        <Confirm
          title="Restore this backup?"
          body={
            <>
              <p>
                The server's files will be replaced with the backup from <strong>{new Date(restoring.created_at_ms).toLocaleString()}</strong>
                {restoring.scope === "world" ? " (worlds and settings only; mods, plugins and other files stay as they are)" : ""}.
              </p>
              <p className="text-muted">Nothing is deleted: your current files are moved aside to a "before-restore" folder first.</p>
            </>
          }
          confirmLabel="Restore"
          onClose={() => setRestoring(null)}
          onConfirm={() => restore(restoring)}
        />
      )}
      {deleting && (
        <Confirm
          title="Delete this backup?"
          danger
          body={<p>This permanently deletes the backup from {new Date(deleting.created_at_ms).toLocaleString()}.</p>}
          confirmLabel="Delete backup"
          onClose={() => setDeleting(null)}
          onConfirm={() => remove(deleting)}
        />
      )}
    </div>
  );
}
