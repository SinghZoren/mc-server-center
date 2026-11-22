import { useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Instance, type ListedPlayer } from "../lib/api";
import { useTopic } from "../lib/hooks";
import { errorText, since } from "../lib/text";
import { Confirm, Notice, Spinner } from "../components/ui";

const NAME_RE = /^[A-Za-z0-9_]{1,16}$/;

type Pending = { action: "kick" | "ban" | "deop" | "pardon" | "whitelist_remove"; player: string } | null;

/** A blocky monogram instead of fetching skins (no third-party image requests). */
function Avatar({ name }: { name: string }) {
  const hue = [...name].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 360, 7);
  return (
    <span
      aria-hidden
      className="grid h-10 w-10 shrink-0 place-items-center font-display text-base font-bold text-[#140c08]"
      style={{ background: `hsl(${hue} 70% 62%)`, boxShadow: "3px 3px 0 #08090b" }}
    >
      {name.slice(0, 1).toUpperCase()}
    </span>
  );
}

export function PlayersTab({ inst }: { inst: Instance }) {
  const qc = useQueryClient();
  const key = ["players", inst.id];
  const players = useQuery({ queryKey: key, queryFn: () => api.players(inst.id) });
  useTopic(`instance/${inst.id}/players`, () => void qc.invalidateQueries({ queryKey: key }), null);

  const [pending, setPending] = useState<Pending>(null);
  const [reason, setReason] = useState("");
  const [message, setMessage] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [addName, setAddName] = useState("");

  async function moderate(action: string, player: string, why?: string) {
    setBusy(true);
    setMessage(null);
    try {
      const r = await api.moderate(action, inst.id, player, why);
      const where = r.applied === "file" ? " (saved to the server's files; takes effect on next start)" : "";
      setMessage({ tone: "ok", text: `Done: ${action.replace("_", " ")} ${player}${where}` });
      // Server confirms asynchronously; refresh shortly after.
      setTimeout(() => void qc.invalidateQueries({ queryKey: key }), r.applied === "console" ? 700 : 0);
    } catch (e) {
      setMessage({ tone: "error", text: errorText(e) });
    } finally {
      setBusy(false);
      setPending(null);
      setReason("");
    }
  }

  if (players.isPending) return <Spinner label="Loading players" />;
  if (players.isError) return <Notice tone="error">{errorText(players.error)}</Notice>;
  const { online, lists, running } = players.data;
  const isOp = (n: string) => lists.ops.some((o) => o.name.toLowerCase() === n.toLowerCase());

  return (
    <div className="max-w-5xl space-y-8">
      {message && <Notice tone={message.tone}>{message.text}</Notice>}

      <section aria-labelledby="online-h">
        <div className="mb-3 flex items-baseline justify-between">
          <h2 id="online-h" className="font-display text-xl font-semibold">
            Online now <span className="text-muted">{running ? online.length : ""}</span>
          </h2>
          {!running && <span className="text-sm text-muted">Server is offline. You can still manage the lists below.</span>}
        </div>
        {running && online.length === 0 && <p className="text-sm text-muted">Nobody is online yet.</p>}
        <ul className="grid grid-cols-1 gap-3 md:grid-cols-2">
          {online.map((p, i) => (
            <li key={p.name} className="block-card rise flex items-center gap-4 p-4" style={{ animationDelay: `${i * 30}ms` }}>
              <Avatar name={p.name} />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 font-semibold">
                  {p.name}
                  {isOp(p.name) && <span className="border border-ember/50 px-1.5 text-[10px] text-ember uppercase">op</span>}
                </div>
                <div className="text-xs text-muted">{p.joined_at_ms ? `Online for ${since(p.joined_at_ms)}` : "Online"}</div>
              </div>
              <div className="flex gap-2">
                {isOp(p.name) ? (
                  <button className="btn px-2.5 py-1.5 text-xs" disabled={busy} onClick={() => setPending({ action: "deop", player: p.name })}>
                    De-op
                  </button>
                ) : (
                  <button className="btn px-2.5 py-1.5 text-xs" disabled={busy} onClick={() => moderate("op", p.name)}>
                    Op
                  </button>
                )}
                <button className="btn px-2.5 py-1.5 text-xs" disabled={busy} onClick={() => setPending({ action: "kick", player: p.name })}>
                  Kick
                </button>
                <button className="btn btn-danger px-2.5 py-1.5 text-xs" disabled={busy} onClick={() => setPending({ action: "ban", player: p.name })}>
                  Ban
                </button>
              </div>
            </li>
          ))}
        </ul>
      </section>

      <section className="grid grid-cols-1 gap-6 lg:grid-cols-3">
        <ListCard
          title="Operators"
          hint="Operators can use every command in-game."
          players={lists.ops}
          actionLabel="Remove"
          onAction={(n) => setPending({ action: "deop", player: n })}
          busy={busy}
        />
        <ListCard
          title="Whitelist"
          hint={lists.whitelist_enabled ? "On: only these players can join." : "Off: anyone with the address can join."}
          players={lists.whitelist}
          actionLabel="Remove"
          onAction={(n) => setPending({ action: "whitelist_remove", player: n })}
          busy={busy}
          header={
            <button
              className={`btn px-2.5 py-1 text-xs ${lists.whitelist_enabled ? "btn-primary" : ""}`}
              disabled={busy}
              aria-pressed={lists.whitelist_enabled}
              onClick={async () => {
                setBusy(true);
                try {
                  await api.setWhitelist(inst.id, !lists.whitelist_enabled);
                  setTimeout(() => void qc.invalidateQueries({ queryKey: key }), 700);
                } catch (e) {
                  setMessage({ tone: "error", text: errorText(e) });
                } finally {
                  setBusy(false);
                }
              }}
            >
              {lists.whitelist_enabled ? "On" : "Off"}
            </button>
          }
        />
        <ListCard
          title="Banned"
          hint="Banned players can't join."
          players={lists.banned}
          actionLabel="Unban"
          onAction={(n) => setPending({ action: "pardon", player: n })}
          busy={busy}
          showReason
        />
      </section>

      <section className="block-card p-5" aria-labelledby="add-h">
        <h3 id="add-h" className="font-display mb-3 font-semibold">
          Add a player by name
        </h3>
        <form
          className="flex flex-wrap items-end gap-3"
          onSubmit={(e) => {
            e.preventDefault();
          }}
        >
          <div className="w-64">
            <label className="label" htmlFor="add-player">
              Minecraft name
            </label>
            <input
              id="add-player"
              className="field console text-xs"
              value={addName}
              maxLength={16}
              list="known-players"
              onChange={(e) => setAddName(e.target.value.trim())}
              aria-invalid={addName !== "" && !NAME_RE.test(addName)}
            />
            <datalist id="known-players">
              {lists.known.map((k) => (
                <option key={k.name} value={k.name} />
              ))}
            </datalist>
          </div>
          {(["whitelist_add", "op", "ban"] as const).map((a) => (
            <button
              key={a}
              type="button"
              className={`btn ${a === "ban" ? "btn-danger" : ""}`}
              disabled={busy || !NAME_RE.test(addName)}
              onClick={() => (a === "ban" ? setPending({ action: "ban", player: addName }) : moderate(a, addName))}
            >
              {a === "whitelist_add" ? "Add to whitelist" : a === "op" ? "Make operator" : "Ban…"}
            </button>
          ))}
          {addName !== "" && !NAME_RE.test(addName) && (
            <p className="w-full text-xs text-danger">Names are 1–16 letters, numbers or underscores.</p>
          )}
        </form>
      </section>

      {pending && (
        <Confirm
          title={
            {
              kick: `Kick ${pending.player}?`,
              ban: `Ban ${pending.player}?`,
              deop: `Remove operator from ${pending.player}?`,
              pardon: `Unban ${pending.player}?`,
              whitelist_remove: `Remove ${pending.player} from the whitelist?`,
            }[pending.action]
          }
          danger={pending.action === "ban" || pending.action === "kick"}
          body={
            pending.action === "ban" ? (
              <p>They'll be disconnected and can't rejoin until you unban them.</p>
            ) : pending.action === "kick" ? (
              <p>They'll be disconnected but can join again.</p>
            ) : (
              <p>You can change this again at any time.</p>
            )
          }
          confirmLabel={{ kick: "Kick", ban: "Ban", deop: "Remove operator", pardon: "Unban", whitelist_remove: "Remove" }[pending.action]}
          onClose={() => setPending(null)}
          onConfirm={() => moderate(pending.action, pending.player, reason)}
        >
          {(pending.action === "kick" || pending.action === "ban") && (
            <>
              <label className="label" htmlFor="reason">
                Reason (optional, shown to the player)
              </label>
              <input id="reason" className="field" maxLength={200} value={reason} onChange={(e) => setReason(e.target.value.replace(/[\r\n]/g, ""))} />
            </>
          )}
        </Confirm>
      )}
    </div>
  );
}

function ListCard({
  title,
  hint,
  players,
  actionLabel,
  onAction,
  busy,
  header,
  showReason = false,
}: {
  title: string;
  hint: string;
  players: ListedPlayer[];
  actionLabel: string;
  onAction: (name: string) => void;
  busy: boolean;
  header?: ReactNode;
  showReason?: boolean;
}) {
  return (
    <div className="block-card flex flex-col p-5">
      <div className="mb-1 flex items-center justify-between">
        <h3 className="font-display font-semibold">
          {title} <span className="text-muted">{players.length}</span>
        </h3>
        {header}
      </div>
      <p className="mb-3 text-xs text-muted">{hint}</p>
      <ul className="space-y-1.5">
        {players.length === 0 && <li className="text-xs text-faint">None</li>}
        {players.map((p) => (
          <li key={p.name} className="flex items-center justify-between gap-2 text-sm">
            <span className="min-w-0">
              <span className="block truncate">{p.name}</span>
              {showReason && p.reason && <span className="block truncate text-xs text-faint">{p.reason}</span>}
            </span>
            <button className="btn btn-ghost px-2 py-0.5 text-xs text-muted hover:text-ink" disabled={busy} onClick={() => onAction(p.name)}>
              {actionLabel}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
