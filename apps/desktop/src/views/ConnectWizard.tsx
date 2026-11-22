import { useState, type ReactNode } from "react";
import { call, type Instance } from "../lib/api";
import { errorText } from "../lib/text";
import { Confirm, Notice, Spinner } from "../components/ui";

interface Check {
  port: number;
  lan_ipv4: string | null;
  public: { ip: string | null; sources: string[]; disagreement: boolean };
  router_wan_ip: string | null;
  upnp_available: boolean;
  upnp_mapped: boolean;
  verdict: "direct_possible" | "cgnat_likely" | "double_nat_likely" | "unknown";
  firewall_rule: boolean;
}
interface External {
  status: "pass" | "fail" | "not_tested";
  source: string;
  note: string;
}

const VERDICT: Record<Check["verdict"], { tone: "ok" | "warn" | "info"; text: string }> = {
  direct_possible: { tone: "ok", text: "Your router has your public address, so opening a port should let friends connect." },
  cgnat_likely: {
    tone: "warn",
    text: "Your internet provider probably shares one public address between many homes (carrier-grade NAT). Opening ports usually won't work. Options: ask your provider for a public IP, use IPv6, or use Through your VPS below: friends join via a small server you rent, with nothing to open at home.",
  },
  double_nat_likely: {
    tone: "warn",
    text: "There seem to be two routers in a row (for example your provider's box plus your own). The port must be opened on both, or one put in bridge mode.",
  },
  unknown: {
    tone: "info",
    text: "Your router doesn't offer automatic setup (UPnP), so we can't tell in advance whether opening a port will work. Follow the steps and use the test at the end.",
  },
};

/** NET-02..04: firewall, router, honest outside test, address to share. */
export function ConnectWizard({ inst }: { inst: Instance }) {
  const [check, setCheck] = useState<Check | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [external, setExternal] = useState<External | null>(null);
  const [confirm, setConfirm] = useState<"upnp" | "test" | "firewall" | null>(null);
  const running = inst.state === "running";

  async function run<T>(key: string, f: () => Promise<T>): Promise<T | null> {
    setBusy(key);
    setError(null);
    try {
      return await f();
    } catch (e) {
      setError(errorText(e) + (e && typeof e === "object" && "detail" in e ? ` (${(e as { detail: string }).detail})` : ""));
      return null;
    } finally {
      setBusy(null);
    }
  }
  const refresh = () => run("check", () => call<Check>("network.check", { instance: inst.id })).then((c) => c && setCheck(c));

  const shareAddress = check?.public.ip ? (inst.port === 25565 ? check.public.ip : `${check.public.ip}:${inst.port}`) : null;

  return (
    <div className="block-card rise space-y-5 p-6 lg:col-span-2" style={{ animationDelay: "90ms" }}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="font-display text-lg font-semibold">Invite friends over the internet</h2>
          <p className="text-sm text-muted">Friends outside your home need your router to let them in. We'll check what's possible first.</p>
        </div>
        <button className="btn" disabled={busy !== null} onClick={refresh}>
          {busy === "check" ? <Spinner label="Checking" /> : check ? "Check again" : "Check my connection"}
        </button>
      </div>
      {error && <Notice tone="error">{error}</Notice>}
      {check && (
        <>
          <Notice tone={VERDICT[check.verdict].tone}>{VERDICT[check.verdict].text}</Notice>
          <ol className="space-y-4 text-sm">
            <Step n={1} title="Allow it through Windows Firewall" done={check.firewall_rule}>
              <p className="text-muted">Lets other computers reach port {inst.port} on this PC. Windows will ask for permission.</p>
              <button className="btn mt-2 text-xs" disabled={busy !== null} onClick={() => setConfirm("firewall")}>
                {check.firewall_rule ? "Add the rule again" : "Allow through firewall"}
              </button>
            </Step>
            <Step n={2} title="Open the port on your router" done={check.upnp_mapped}>
              {check.upnp_available ? (
                <>
                  <p className="text-muted">Your router supports automatic setup (UPnP).</p>
                  <div className="mt-2 flex gap-2">
                    {check.upnp_mapped ? (
                      <button className="btn text-xs" disabled={busy !== null} onClick={() => run("upnp", () => call("network.upnp_close", { instance: inst.id })).then(refresh)}>
                        Close the port
                      </button>
                    ) : (
                      <button className="btn text-xs" disabled={busy !== null} onClick={() => setConfirm("upnp")}>
                        Open port {inst.port} automatically…
                      </button>
                    )}
                  </div>
                </>
              ) : (
                <p className="text-muted">
                  In your router's settings (often at <span className="console text-xs">192.168.0.1</span> or{" "}
                  <span className="console text-xs">192.168.1.1</span>), add a <strong className="text-ink">port forward</strong>: protocol{" "}
                  <span className="console text-xs text-ink">TCP</span>, port <span className="console text-xs text-ink">{inst.port}</span>, to{" "}
                  <span className="console text-xs text-ink">{check.lan_ipv4 ?? "this PC"}</span>.
                </p>
              )}
            </Step>
            <Step n={3} title="Test from the internet" done={external?.status === "pass"}>
              {!running && <p className="text-muted">Start the server first.</p>}
              <button className="btn mt-1 text-xs" disabled={!running || busy !== null} onClick={() => setConfirm("test")}>
                {busy === "test" ? <Spinner /> : "Run the outside test…"}
              </button>
              {external && (
                <p className={`mt-2 text-sm ${external.status === "pass" ? "text-moss" : external.status === "fail" ? "text-danger" : "text-muted"}`}>
                  {external.status === "pass" ? "✓ Reachable from the internet." : external.status === "fail" ? "✗ Not reachable from the internet yet." : "Not tested."}{" "}
                  <span className="text-xs text-faint">{external.note}</span>
                </p>
              )}
            </Step>
            <Step n={4} title="Share your address" done={false}>
              {shareAddress ? (
                <>
                  <code className="console bg-[#0f1013] px-2 py-1 text-ember select-all">{shareAddress}</code>
                  {external?.status !== "pass" && <p className="mt-1 text-xs text-amber">Not confirmed yet: run the outside test first.</p>}
                  <p className="mt-1 text-xs text-faint">Home addresses can change; a custom domain (coming soon) keeps one name for friends.</p>
                </>
              ) : (
                <p className="text-muted">Your public address couldn't be determined{check.public.disagreement ? " (two sources disagreed)" : ""}.</p>
              )}
            </Step>
          </ol>
        </>
      )}

      {confirm === "firewall" && (
        <Confirm
          title="Allow through Windows Firewall?"
          body={<p>Windows will show an admin prompt to add one rule: incoming TCP connections on port {inst.port}. You can remove it later.</p>}
          confirmLabel="Continue"
          onClose={() => setConfirm(null)}
          onConfirm={() => {
            setConfirm(null);
            void run("firewall", () => call("network.firewall_allow", { instance: inst.id })).then(refresh);
          }}
        />
      )}
      {confirm === "upnp" && (
        <Confirm
          title="Open the port on your router?"
          body={
            <>
              <p>Your router will forward internet traffic on TCP port {inst.port} to this PC. Anyone who knows your address can then reach the Minecraft server (and nothing else).</p>
              <p className="text-muted">You can close it again here at any time.</p>
            </>
          }
          confirmLabel="Open port"
          onClose={() => setConfirm(null)}
          onConfirm={() => {
            setConfirm(null);
            void run("upnp", () => call("network.upnp_open", { instance: inst.id })).then(refresh);
          }}
        />
      )}
      {confirm === "test" && (
        <Confirm
          title="Test from the internet?"
          body={
            <>
              <p>To check from outside your home, your public address and port are sent to <strong>mcsrvstat.us</strong>, a free third-party checker. It connects back like a player would.</p>
              <p className="text-muted">Results can be up to 5 minutes old.</p>
            </>
          }
          confirmLabel="Run test"
          onClose={() => setConfirm(null)}
          onConfirm={() => {
            setConfirm(null);
            void run("test", () => call<External>("network.external_test", { instance: inst.id, consent: true })).then((r) => r && setExternal(r));
          }}
        />
      )}
    </div>
  );
}

function Step({ n, title, done, children }: { n: number; title: string; done: boolean; children: ReactNode }) {
  return (
    <li className="flex gap-4">
      <span
        aria-hidden
        className={`grid h-7 w-7 shrink-0 place-items-center text-xs font-bold ${done ? "bg-moss text-[#0b1a11]" : "bg-panel-2 text-muted"}`}
      >
        {done ? "✓" : n}
      </span>
      <div className="min-w-0 flex-1">
        <h3 className="font-semibold">{title}</h3>
        <div className="mt-1">{children}</div>
      </div>
    </li>
  );
}
