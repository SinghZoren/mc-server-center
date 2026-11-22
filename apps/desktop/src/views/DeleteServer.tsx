import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, call, type Instance } from "../lib/api";
import { errorText } from "../lib/text";
import { Confirm, Notice, Spinner } from "../components/ui";

const STOPPED = ["stopped", "crashed", "install_failed"];

/**
 * "Danger zone" at the bottom of a server's Settings: deleting needs the server stopped, a
 * dialog, and the server's exact name typed in, so it can't happen by accident.
 */
export function DeleteServerSection({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);
  const stopped = STOPPED.includes(inst.state);
  const matches = typed === inst.name;

  function close() {
    if (busy) return;
    setOpen(false);
    setTyped("");
    setError(null);
  }

  async function stop() {
    setStopping(true);
    setError(null);
    try {
      await api.stop(inst.id);
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } catch (e) {
      setError(errorText(e));
    } finally {
      setStopping(false);
    }
  }

  async function remove() {
    if (!matches || busy) return;
    setBusy(true);
    setError(null);
    try {
      await call("instances.delete", { instance: inst.id, confirm: true });
      setOpen(false);
      // The server list refreshes and the app moves to another server.
      void qc.invalidateQueries({ queryKey: ["instances"] });
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-labelledby="danger-zone">
      <h2 id="danger-zone" className="label mb-3 text-danger">
        Danger zone
      </h2>
      <div
        className="block-card flex flex-wrap items-center justify-between gap-4 px-5 py-4"
        style={{ borderColor: "color-mix(in srgb, var(--color-danger) 55%, transparent)" }}
      >
        <div className="min-w-0 flex-1 basis-64">
          <p className="font-semibold">Delete this server</p>
          <p className="mt-0.5 text-xs text-muted">
            Removes {inst.name} from the app, with its worlds, mods and backups.
            {stopped ? "" : " Stop the server first."}
          </p>
        </div>
        {stopped ? (
          <button className="btn btn-danger" onClick={() => setOpen(true)}>
            Delete server…
          </button>
        ) : (
          <button className="btn" disabled={stopping || inst.state === "stopping"} onClick={stop}>
            {stopping || inst.state === "stopping" ? <Spinner label="Stopping" /> : "Stop the server"}
          </button>
        )}
      </div>
      {!open && error && (
        <div className="mt-3">
          <Notice tone="error">{error}</Notice>
        </div>
      )}

      {open && (
        <Confirm
          title={`Delete ${inst.name}?`}
          danger
          confirmLabel={busy ? <Spinner label="Deleting" /> : "Delete server"}
          confirmDisabled={!matches || busy || !stopped}
          onClose={close}
          onConfirm={remove}
          body={
            <>
              <p>
                <strong>{inst.name}</strong> and everything in it (worlds, mods or plugins, settings and backups) will be removed from MC Server
                Center. Friends won't be able to join it any more.
              </p>
              <p className="text-muted">
                As a safety net the folder is moved to the trash folder inside the app's data folder rather than erased, so it can still be
                recovered by hand.
              </p>
            </>
          }
        >
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void remove();
            }}
          >
            <label className="label" htmlFor="delete-confirm-name">
              Type <span className="console normal-case text-ink">{inst.name}</span> to confirm
            </label>
            <input
              id="delete-confirm-name"
              className="field"
              autoComplete="off"
              spellCheck={false}
              value={typed}
              disabled={busy}
              onChange={(e) => setTyped(e.target.value)}
              onPaste={(e) => e.preventDefault()}
              aria-invalid={typed.length > 0 && !matches}
            />
            {typed.length > 0 && !matches && <p className="mt-1.5 text-xs text-muted">The name doesn't match yet (capital letters count).</p>}
            {!stopped && <p className="mt-1.5 text-xs text-amber">The server started again. Stop it before deleting.</p>}
          </form>
          {error && (
            <div className="mt-3">
              <Notice tone="error">{error}</Notice>
            </div>
          )}
        </Confirm>
      )}
    </section>
  );
}
