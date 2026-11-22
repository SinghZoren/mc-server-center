import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { hostsApi, type HostInfo } from "../lib/api";
import { errorText } from "../lib/text";
import { Modal, Notice, Spinner } from "./ui";

const INSTALL_CMD = "curl -fsSL https://github.com/SinghZoren/mc-server-center/releases/latest/download/install-linux.sh | sudo sh";

/** Choose which machine to manage: this PC or a paired remote server (REM-03). */
export function HostSwitcher({ host, onChange }: { host: string; onChange: (id: string) => void }) {
  const qc = useQueryClient();
  const hosts = useQuery({ queryKey: ["hosts"], queryFn: hostsApi.list });
  const [adding, setAdding] = useState(false);
  const list: HostInfo[] = hosts.data ?? [{ id: "local", name: "This PC", address: null }];
  return (
    <div className="px-5 pb-3">
      <label className="label" htmlFor="host-select">
        Managing
      </label>
      <select
        id="host-select"
        className="field py-1.5 text-sm"
        value={host}
        onChange={(e) => (e.target.value === "__add" ? setAdding(true) : onChange(e.target.value))}
      >
        {list.map((h) => (
          <option key={h.id} value={h.id}>
            {h.name}
            {h.address ? ` (${h.address})` : ""}
          </option>
        ))}
        <option value="__add">+ Add a VPS / remote server…</option>
      </select>
      {list.length === 1 && (
        <button className="mt-2 text-left text-xs text-muted underline-offset-2 hover:text-ink hover:underline" onClick={() => setAdding(true)}>
          Run servers on a VPS, online 24/7 →
        </button>
      )}
      {adding && (
        <AddHost
          onClose={() => setAdding(false)}
          onAdded={(id) => {
            setAdding(false);
            void qc.invalidateQueries({ queryKey: ["hosts"] });
            onChange(id);
          }}
        />
      )}
    </div>
  );
}

export function AddHost({ onClose, onAdded }: { onClose: () => void; onAdded: (id: string) => void }) {
  const [address, setAddress] = useState("");
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Modal title="Run servers on a VPS or another Linux machine" onClose={onClose} width={640}>
      <div className="space-y-4 text-sm">
        <p className="text-muted">
          A VPS keeps your servers online 24/7 without your PC, and friends connect to it directly (no router setup). Any x86-64 Linux machine with
          systemd works: a rented VPS (4 GB of memory or more is comfortable) or an old PC.
        </p>
        <div>
          <p className="label">1. On the Linux machine, run</p>
          <div className="flex items-start gap-2">
            <code className="console block flex-1 bg-panel-2 px-3 py-2 text-[11px] break-all text-ink">{INSTALL_CMD}</code>
            <button className="btn btn-ghost px-2 py-1 text-xs" onClick={() => void navigator.clipboard.writeText(INSTALL_CMD)}>
              Copy
            </button>
          </div>
          <p className="mt-1 text-xs text-faint">
            It installs MC Server Center as a service, turns on remote control, and prints an address and a pairing code. Also allow TCP 47625 and your
            Minecraft ports (25565…) in the VPS provider's firewall if it has one.
          </p>
        </div>
        <p className="label">2. Enter what it printed</p>
        <div>
          <label className="label" htmlFor="host-addr">Address and port</label>
          <input id="host-addr" className="field console text-xs" placeholder="203.0.113.5:47625" value={address} onChange={(e) => setAddress(e.target.value.trim())} />
        </div>
        <div>
          <label className="label" htmlFor="host-code">Pairing code</label>
          <input id="host-code" className="field console text-xs uppercase" placeholder="XXXXX-XXXXX-XXXXX-XXXXX" value={code} onChange={(e) => setCode(e.target.value)} />
        </div>
        <div>
          <label className="label" htmlFor="host-name">Name (optional)</label>
          <input id="host-name" className="field" maxLength={64} placeholder="My VPS" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <Notice>The connection is encrypted, and both sides check each other's identity. Nothing is sent to any other service.</Notice>
        {error && <Notice tone="error">{error}</Notice>}
        <div className="flex justify-end gap-3">
          <button className="btn" onClick={onClose}>Cancel</button>
          <button
            className="btn btn-primary"
            disabled={busy || !address.includes(":") || code.replace(/[^A-Za-z0-9]/g, "").length !== 20}
            onClick={async () => {
              setBusy(true);
              setError(null);
              try {
                const h = await hostsApi.pair(address, code, name);
                onAdded(h.id);
              } catch (e) {
                setError(errorText(e) + (e && typeof e === "object" && "detail" in e ? ` — ${(e as { detail: string }).detail}` : ""));
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? <Spinner label="Pairing" /> : "Pair"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
