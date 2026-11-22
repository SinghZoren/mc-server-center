import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { call, isApiError, type Instance } from "../lib/api";
import { errorText } from "../lib/text";
import { Confirm, Modal, Notice, Spinner } from "../components/ui";

interface Link {
  hostname: string;
  provider: "cloudflare" | "manual";
  zone_name?: string;
  last_ip?: string;
  updated_at?: number;
  last_error?: string | null;
}
interface Status {
  cloudflare_connected: boolean;
  link: Link | null;
  share_address: string | null;
}
interface PlannedRecord {
  kind: string;
  name: string;
  value: string;
  note: string;
  /** The record split into the boxes most providers' forms have (older services omit it). */
  fields?: { label: string; value: string; hint: string | null }[];
}
interface DnsCheck {
  a_ok: boolean;
  srv_ok: boolean;
  srv_needed: boolean;
  summary: string;
}
interface Zone {
  id: string;
  name: string;
}

/** Shows the error detail too: domain errors are specific and actionable. */
function describe(e: unknown): string {
  if (isApiError(e) && e.detail && !e.detail.startsWith("unknown command")) return e.detail;
  return errorText(e);
}

function Copy({ text }: { text: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className="btn btn-ghost px-2 py-1 text-xs"
      onClick={() => {
        void navigator.clipboard.writeText(text);
        setDone(true);
        setTimeout(() => setDone(false), 1500);
      }}
    >
      {done ? "Copied" : "Copy"}
    </button>
  );
}

/** "Your own address": link a domain the user owns to this server (Cloudflare or any provider). */
export function DomainCard({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["domains", inst.id];
  const status = useQuery({ queryKey: key, queryFn: () => call<Status>("domains.status", { instance: inst.id }) });
  const [setup, setSetup] = useState(false);
  const [check, setCheck] = useState<DnsCheck | null>(null);
  const [records, setRecords] = useState<PlannedRecord[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unlinking, setUnlinking] = useState(false);
  const link = status.data?.link ?? null;

  async function run(k: string, f: () => Promise<void>) {
    setBusy(k);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="block-card rise space-y-4 p-6 lg:col-span-2" style={{ animationDelay: "110ms" }}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="font-display text-lg font-semibold">Your own address</h2>
          <p className="text-sm text-muted">
            Give friends a name like <span className="console text-xs text-ink">play.yourdomain.com</span> instead of a number, using a domain you own.
          </p>
        </div>
        {!link && status.data && (
          <button className="btn" onClick={() => setSetup(true)}>
            Use my domain…
          </button>
        )}
      </div>

      {status.isPending && <Spinner label="Loading" />}
      {status.isError && <Notice tone="error">{describe(status.error)}</Notice>}

      {link && (
        <div className="space-y-3">
          <div className="flex flex-wrap items-center gap-3">
            <span className="console rounded-none bg-panel-2 px-3 py-2 text-sm text-ember">{status.data?.share_address}</span>
            {status.data?.share_address && <Copy text={status.data.share_address} />}
          </div>
          <p className="text-xs text-faint">
            {link.provider === "cloudflare"
              ? `Managed in Cloudflare (${link.zone_name}). If your internet address changes, the record is updated automatically within about 10 minutes.`
              : "You manage these records at your domain provider. If your internet address changes, update the A record."}
            {link.last_ip ? ` Points to ${link.last_ip}.` : ""}
          </p>
          {link.last_error && <Notice tone="warn">The last automatic update failed: {link.last_error}</Notice>}
          {check && <Notice tone={check.a_ok && check.srv_ok ? "ok" : "warn"}>{check.summary}</Notice>}
          {records && <RecordTable records={records} />}
          <div className="flex flex-wrap gap-3">
            <button
              className="btn"
              disabled={!!busy}
              onClick={() => run("check", async () => setCheck(await call<DnsCheck>("domains.check", { instance: inst.id })))}
            >
              {busy === "check" ? <Spinner label="Checking" /> : "Check it works"}
            </button>
            {link.provider === "manual" && (
              <button
                className="btn btn-ghost"
                disabled={!!busy}
                onClick={() =>
                  run("records", async () => setRecords((await call<{ records: PlannedRecord[] }>("domains.records", { instance: inst.id })).records))
                }
              >
                {busy === "records" ? <Spinner label="Loading" /> : "Show the records again"}
              </button>
            )}
            <button className="btn btn-ghost text-muted" disabled={!!busy} onClick={() => setUnlinking(true)}>
              Remove
            </button>
          </div>
          <p className="text-xs text-faint">
            The address only works once friends can reach this server from the internet: see "Invite friends over the internet" above.
          </p>
        </div>
      )}

      {error && <Notice tone="error">{error}</Notice>}

      {setup && status.data && (
        <DomainSetup
          inst={inst}
          cloudflareConnected={status.data.cloudflare_connected}
          onClose={() => setSetup(false)}
          onLinked={(r) => {
            setSetup(false);
            setRecords(r);
            setCheck(null);
            void qc.invalidateQueries({ queryKey: key });
          }}
        />
      )}
      {unlinking && link && (
        <Confirm
          title="Remove this address?"
          confirmLabel="Remove"
          danger
          onClose={() => setUnlinking(false)}
          body={
            link.provider === "cloudflare"
              ? `The DNS records for ${link.hostname} that this app created in Cloudflare will be deleted.`
              : `This app stops showing ${link.hostname}. Delete the records at your domain provider yourself if you no longer need them.`
          }
          onConfirm={() =>
            void run("unlink", async () => {
              setUnlinking(false);
              await call("domains.unlink", { instance: inst.id });
              setCheck(null);
              setRecords(null);
              await qc.invalidateQueries({ queryKey: key });
            })
          }
        />
      )}
    </div>
  );
}

function RecordTable({ records }: { records: PlannedRecord[] }) {
  return (
    <div className="space-y-3">
      <p className="text-sm text-muted">
        At your domain provider, open its "DNS" or "DNS records" page and add {records.length === 1 ? "this record" : `these ${records.length} records`}.
        Fill each box with the value shown under the same name:
      </p>
      {records.map((r, i) => (
        <RecordFields key={r.kind + r.name} record={r} index={i + 1} />
      ))}
      <p className="text-xs text-faint">Changes can take a few minutes to appear; then use "Check it works".</p>
    </div>
  );
}

function RecordFields({ record: r, index }: { record: PlannedRecord; index: number }) {
  const [oneLine, setOneLine] = useState(false);
  return (
    <div className="border border-line bg-panel-2/40 p-4">
      <p className="mb-3 text-sm">
        <span className="font-semibold">
          {index}. {r.kind} record
        </span>{" "}
        <span className="text-muted">· {r.note}</span>
      </p>
      <dl className="grid grid-cols-[repeat(auto-fill,minmax(11rem,1fr))] gap-x-4 gap-y-3">
        {(
          r.fields ?? [
            { label: "Name", value: r.name, hint: null },
            { label: "Value", value: r.value, hint: null },
          ]
        ).map((f) => (
          <div key={f.label} className="min-w-0">
            <dt className="label mb-1">{f.label}</dt>
            <dd className="flex items-center gap-1">
              <span className="console min-w-0 bg-panel px-2 py-1 text-xs break-all text-ink">
                {f.value}
              </span>
              {f.label !== "Type" && f.label !== "TTL" && <Copy text={f.value} />}
            </dd>
            {f.hint && <p className="mt-1 text-[11px] leading-snug text-faint">{f.hint}</p>}
          </div>
        ))}
      </dl>
      {r.kind === "SRV" && (
        <div className="mt-3">
          <button className="text-xs text-faint underline-offset-2 hover:text-ink hover:underline" onClick={() => setOneLine((v) => !v)}>
            {oneLine ? "Hide" : "My provider only has a Name and a Value box"}
          </button>
          {oneLine && (
            <dl className="mt-2 grid grid-cols-1 gap-2 text-xs sm:grid-cols-2">
              <div>
                <dt className="label mb-1">Name</dt>
                <dd className="console flex items-center gap-1">
                  <span className="min-w-0 truncate">{r.name}</span> <Copy text={r.name} />
                </dd>
              </div>
              <div>
                <dt className="label mb-1">Value (priority weight port target)</dt>
                <dd className="console flex items-center gap-1">
                  <span className="min-w-0 truncate">{r.value}</span> <Copy text={r.value} />
                </dd>
              </div>
            </dl>
          )}
        </div>
      )}
    </div>
  );
}

function DomainSetup({
  inst,
  cloudflareConnected,
  onClose,
  onLinked,
}: {
  inst: Instance;
  cloudflareConnected: boolean;
  onClose: () => void;
  onLinked: (records: PlannedRecord[] | null) => void;
}) {
  const [provider, setProvider] = useState<"cloudflare" | "manual">("cloudflare");
  const [token, setToken] = useState("");
  const [connected, setConnected] = useState(cloudflareConnected);
  const zones = useQuery({
    queryKey: ["cf-zones"],
    queryFn: () => call<{ zones: Zone[] }>("domains.zones"),
    enabled: connected && provider === "cloudflare",
  });
  const [hostname, setHostname] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const domain = zones.data?.zones[0]?.name ?? "yourdomain.com";

  async function connect() {
    setBusy(true);
    setError(null);
    try {
      await call("domains.set_token", { token });
      setToken("");
      setConnected(true);
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(false);
    }
  }

  async function link() {
    setBusy(true);
    setError(null);
    try {
      const r = await call<{ records: PlannedRecord[] }>("domains.link", { instance: inst.id, hostname, provider });
      onLinked(provider === "manual" ? r.records : null);
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title="Use your own domain" onClose={onClose} width={620}>
      <div className="space-y-5 text-sm">
        <div className="grid grid-cols-2 gap-3" role="radiogroup" aria-label="Where your domain's DNS is managed">
          {(
            [
              ["cloudflare", "My domain uses Cloudflare", "Set up automatically, and kept up to date if your internet address changes."],
              ["manual", "Another provider", "GoDaddy, Namecheap, Porkbun, Google, … We'll show exactly what to add."],
            ] as const
          ).map(([id, title, text]) => (
            <button
              key={id}
              role="radio"
              aria-checked={provider === id}
              className={`block-card p-4 text-left ${provider === id ? "outline outline-1 outline-ember" : ""}`}
              onClick={() => setProvider(id)}
            >
              <span className="block font-semibold">{title}</span>
              <span className="mt-1 block text-xs text-muted">{text}</span>
            </button>
          ))}
        </div>

        {provider === "cloudflare" && !connected && (
          <div className="space-y-3">
            <ol className="list-decimal space-y-1 pl-5 text-muted">
              <li>
                In Cloudflare, open <span className="text-ink">My Profile → API Tokens → Create Token</span>.
              </li>
              <li>
                Use the <span className="text-ink">Edit zone DNS</span> template, and under Zone Resources pick your domain.
              </li>
              <li>Create the token and paste it below. It's stored only on the machine running this server.</li>
            </ol>
            <div>
              <label className="label" htmlFor="cf-token">
                Cloudflare API token
              </label>
              <input
                id="cf-token"
                className="field console text-xs"
                type="password"
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value.trim())}
              />
            </div>
            <div className="flex justify-end">
              <button className="btn btn-primary" disabled={busy || token.length < 20} onClick={() => void connect()}>
                {busy ? <Spinner label="Checking the token" /> : "Connect Cloudflare"}
              </button>
            </div>
          </div>
        )}

        {(provider === "manual" || connected) && (
          <div className="space-y-3">
            {provider === "cloudflare" && zones.data && (
              <p className="text-xs text-muted">Connected. Domains this token can edit: {zones.data.zones.map((z) => z.name).join(", ") || "none"}.</p>
            )}
            {provider === "cloudflare" && zones.isError && <Notice tone="error">{describe(zones.error)}</Notice>}
            <div>
              <label className="label" htmlFor="dom-host">
                Address for this server
              </label>
              <input
                id="dom-host"
                className="field console text-xs"
                placeholder={`play.${domain}`}
                value={hostname}
                onChange={(e) => setHostname(e.target.value)}
              />
              <p className="mt-1 text-xs text-faint">
                A name under a domain you own, such as <span className="console">play.{domain}</span>, or the domain itself.
                {inst.port !== 25565 ? ` Because this server uses port ${inst.port}, we also add an SRV record so friends don't need to type the port.` : ""}
              </p>
            </div>
            <div className="flex justify-end gap-3">
              <button className="btn" onClick={onClose}>
                Cancel
              </button>
              <button className="btn btn-primary" disabled={busy || hostname.trim().length < 3} onClick={() => void link()}>
                {busy ? <Spinner label="Working" /> : provider === "cloudflare" ? "Create the records" : "Show me what to add"}
              </button>
            </div>
          </div>
        )}

        <Notice>
          A domain gives your server a name; it doesn't open your network. If friends can't reach you by IP address, they can't by name either: use
          "Invite friends over the internet" first, or run the server on a VPS.
        </Notice>
        {error && <Notice tone="error">{error}</Notice>}
      </div>
    </Modal>
  );
}
