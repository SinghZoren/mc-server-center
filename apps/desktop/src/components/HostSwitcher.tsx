import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { hostsApi, type HostInfo } from "../lib/api";
import { errorText } from "../lib/text";
import { Modal, Notice, Spinner } from "./ui";

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
        <option value="__add">+ Add a remote server…</option>
      </select>
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

function AddHost({ onClose, onAdded }: { onClose: () => void; onAdded: (id: string) => void }) {
  const [address, setAddress] = useState("");
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Modal title="Add a remote server" onClose={onClose} width={560}>
      <div className="space-y-4 text-sm">
        <ol className="list-decimal space-y-1 pl-5 text-muted">
          <li>Install MC Server Center on the Linux machine (see the Linux guide).</li>
          <li>
            On that machine run <code className="console text-xs text-ink">msmctl remote-on</code>, then{" "}
            <code className="console text-xs text-ink">msmctl pair-code "My PC"</code>.
          </li>
          <li>Enter its address and the code below. Codes work once and expire after 10 minutes.</li>
        </ol>
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
          <input id="host-name" className="field" maxLength={64} placeholder="Home server" value={name} onChange={(e) => setName(e.target.value)} />
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
