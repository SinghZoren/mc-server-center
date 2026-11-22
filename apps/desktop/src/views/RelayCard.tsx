import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { call, callOn, getHost, hostsApi, isApiError, type Instance } from "../lib/api";
import { errorText } from "../lib/text";
import { Confirm, Notice, Spinner } from "../components/ui";
import { AddHost } from "../components/HostSwitcher";

interface RelayPort {
  instance: string;
  port: number;
  share_address: string;
  open: boolean;
  error: string | null;
}
interface RelayStatus {
  configured: boolean;
  address?: string;
  host?: string;
  connected?: boolean;
  last_error?: string | null;
  ports?: RelayPort[];
  active_connections?: number;
}

function describe(e: unknown): string {
  if (isApiError(e) && e.detail && !e.detail.startsWith("unknown command")) return e.detail;
  return errorText(e);
}

/**
 * "Through your VPS": friends connect to a VPS you rent, which forwards to this server over an
 * encrypted connection that this machine opens (TUN-01). Nothing to open at home; works behind
 * carrier-grade NAT.
 */
export function RelayCard({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["relay"];
  const status = useQuery({ queryKey: key, queryFn: () => call<RelayStatus>("relay.status"), refetchInterval: 10_000 });
  const hosts = useQuery({ queryKey: ["hosts"], queryFn: hostsApi.list });
  const vpsChoices = (hosts.data ?? []).filter((h) => h.id !== "local" && h.id !== getHost() && h.address);
  const [vps, setVps] = useState<string>("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [port, setPort] = useState<string>(String(inst.port));

  const s = status.data;
  const mine = s?.ports?.find((p) => p.instance === inst.id) ?? null;

  async function run(k: string, f: () => Promise<void>) {
    setBusy(k);
    setError(null);
    try {
      await f();
      await qc.invalidateQueries({ queryKey: key });
      await qc.invalidateQueries({ queryKey: ["domains", inst.id] });
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  async function setUp() {
    const host = vpsChoices.find((h) => h.id === (vps || vpsChoices[0]?.id));
    if (!host?.address) return;
    // 1. Ask the VPS for a one-time code (over the app's existing, pinned connection to it).
    const grant = await callOn<{ code: string; fingerprint: string }>(host.id, "relay.create_code", { name: inst.name });
    // 2. This machine pairs with the VPS using it; only relay rights are granted.
    await call("relay.setup", { address: host.address, fingerprint: grant.fingerprint, code: grant.code });
    // 3. Relay this server.
    await call("relay.expose", { instance: inst.id, port: Number(port) || inst.port });
  }

  return (
    <div className="block-card rise space-y-4 p-6 lg:col-span-2" style={{ animationDelay: "100ms" }}>
      <div>
        <h2 className="font-display text-lg font-semibold">Through your VPS</h2>
        <p className="text-sm text-muted">
          Friends connect to a small server you rent, which passes them on to this one. Nothing to open at home, and it works even when your
          internet provider blocks incoming connections.
        </p>
      </div>

      {status.isPending && <Spinner label="Loading" />}
      {status.isError && <Notice tone="error">{describe(status.error)}</Notice>}

      {s && !s.configured && (
        <div className="space-y-3">
          {vpsChoices.length === 0 ? (
            <>
              <p className="text-sm text-muted">First, add your VPS to the app (one command on the VPS, then a pairing code).</p>
              <button className="btn" onClick={() => setAdding(true)}>
                Add a VPS…
              </button>
            </>
          ) : (
            <div className="flex flex-wrap items-end gap-3">
              <div>
                <label className="label" htmlFor="relay-vps">
                  VPS
                </label>
                <select id="relay-vps" className="field py-1.5 text-sm" value={vps || vpsChoices[0]?.id} onChange={(e) => setVps(e.target.value)}>
                  {vpsChoices.map((h) => (
                    <option key={h.id} value={h.id}>
                      {h.name} ({h.address})
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <label className="label" htmlFor="relay-port">
                  Port on the VPS
                </label>
                <input id="relay-port" className="field console w-28 text-xs" inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
              </div>
              <button className="btn btn-primary" disabled={!!busy} onClick={() => void run("setup", setUp)}>
                {busy === "setup" ? <Spinner label="Setting up" /> : "Relay this server"}
              </button>
            </div>
          )}
        </div>
      )}

      {s?.configured && (
        <div className="space-y-3">
          <p className="text-sm">
            <span
              className="mr-2 inline-block h-1.5 w-1.5 align-middle"
              style={{ background: s.connected ? "var(--color-moss)" : "var(--color-danger)" }}
            />
            {s.connected ? `Connected to the relay at ${s.host}` : `Not connected to the relay at ${s.host}`}
            {s.connected && s.active_connections ? ` · ${s.active_connections} player connection(s) now` : ""}
          </p>
          {!s.connected && s.last_error && <Notice tone="warn">{s.last_error}</Notice>}

          {mine ? (
            <>
              <div className="flex flex-wrap items-center gap-3">
                <span className="console bg-panel-2 px-3 py-2 text-sm text-ember">{mine.share_address}</span>
                <button className="btn btn-ghost px-2 py-1 text-xs" onClick={() => void navigator.clipboard.writeText(mine.share_address)}>
                  Copy
                </button>
              </div>
              {mine.error ? (
                <Notice tone="error">The VPS couldn't open port {mine.port}: {mine.error}</Notice>
              ) : (
                <p className="text-xs text-faint">
                  Give friends this address. Allow TCP {mine.port} in your VPS provider's firewall if it has one. Players appear to the server as
                  connecting from this PC.
                </p>
              )}
              <button className="btn btn-ghost text-muted" disabled={!!busy} onClick={() => void run("unexpose", async () => void (await call("relay.unexpose", { instance: inst.id })))}>
                Stop relaying this server
              </button>
            </>
          ) : (
            <div className="flex flex-wrap items-end gap-3">
              <div>
                <label className="label" htmlFor="relay-port2">
                  Port on the VPS
                </label>
                <input id="relay-port2" className="field console w-28 text-xs" inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
              </div>
              <button
                className="btn btn-primary"
                disabled={!!busy}
                onClick={() => void run("expose", async () => void (await call("relay.expose", { instance: inst.id, port: Number(port) || inst.port })))}
              >
                {busy === "expose" ? <Spinner label="Opening" /> : "Relay this server"}
              </button>
            </div>
          )}
          <button className="text-xs text-faint underline-offset-2 hover:text-ink hover:underline" onClick={() => setRemoving(true)}>
            Disconnect from this VPS
          </button>
        </div>
      )}

      {error && <Notice tone="error">{error}</Notice>}

      {adding && (
        <AddHost
          onClose={() => setAdding(false)}
          onAdded={() => {
            setAdding(false);
            void qc.invalidateQueries({ queryKey: ["hosts"] });
          }}
        />
      )}
      {removing && (
        <Confirm
          title="Disconnect from the VPS?"
          confirmLabel="Disconnect"
          danger
          onClose={() => setRemoving(false)}
          body="Friends can no longer join any of this machine's servers through the VPS. You can set it up again at any time."
          onConfirm={() => {
            setRemoving(false);
            void run("remove", async () => void (await call("relay.remove")));
          }}
        />
      )}
    </div>
  );
}
