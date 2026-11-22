import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api, type Instance } from "../lib/api";
import { pushLine, useConsole, type ConsoleLine as Line } from "../lib/consoleStore";
import { errorText } from "../lib/text";

function lineTone(l: Line): string {
  if (l.kind === "gap") return "text-faint italic";
  if (l.kind === "me") return "text-ember";
  if (l.kind === "err" || /\/(ERROR|FATAL)\]/.test(l.text)) return "text-danger";
  if (/\/WARN\]/.test(l.text)) return "text-amber";
  if (/joined the game|left the game/.test(l.text)) return "text-moss";
  return "text-ink/85";
}

/** Live console. Text is rendered as plain text nodes only (never as HTML). */
export function ConsoleTab({ inst }: { inst: Instance }) {
  const lines = useConsole(inst.id);
  const [input, setInput] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [historyIdx, setHistoryIdx] = useState(-1);
  const [error, setError] = useState<string | null>(null);
  const [follow, setFollow] = useState(true);
  const box = useRef<HTMLDivElement>(null);

  useLayoutEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines, follow]);

  useEffect(() => setError(null), [inst.state]);

  const running = inst.state === "running" || inst.state === "starting";

  async function send() {
    const line = input.trim().replace(/^\//, "");
    if (!line) return;
    setError(null);
    try {
      await api.send(inst.id, line);
      pushLine(inst.id, { seq: Date.now(), text: `> ${line}`, kind: "me" });
      setHistory((h) => [line, ...h.filter((x) => x !== line)].slice(0, 50));
      setHistoryIdx(-1);
      setInput("");
    } catch (e) {
      setError(errorText(e));
    }
  }

  return (
    <div className="flex h-full min-h-[480px] flex-col gap-3">
      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget;
          setFollow(el.scrollHeight - el.scrollTop - el.clientHeight < 40);
        }}
        className="console block-card min-h-0 flex-1 overflow-y-auto bg-[#0c0d10] px-4 py-3 whitespace-pre-wrap"
        role="log"
        aria-live="off"
        aria-label="Server console output"
      >
        {lines.length === 0 && <p className="text-faint">{running ? "Waiting for output…" : "Start the server to see its console here."}</p>}
        {lines.map((l, i) => (
          <div key={`${l.seq}-${i}`} className={lineTone(l)}>
            {l.text}
          </div>
        ))}
      </div>
      <form
        className="flex gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
      >
        <label htmlFor="console-input" className="sr-only">
          Console command
        </label>
        <div className="flex flex-1 items-center border border-line bg-[#0f1013] focus-within:border-ember">
          <span className="console pl-3 text-ember" aria-hidden>
            /
          </span>
          <input
            id="console-input"
            className="console w-full bg-transparent px-2 py-2.5 text-xs outline-none"
            placeholder={running ? "Type a command, e.g. say hello" : "Server is not running"}
            disabled={!running}
            value={input}
            maxLength={1000}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowUp" && history.length) {
                e.preventDefault();
                const i = Math.min(historyIdx + 1, history.length - 1);
                setHistoryIdx(i);
                setInput(history[i] ?? "");
              } else if (e.key === "ArrowDown") {
                e.preventDefault();
                const i = historyIdx - 1;
                setHistoryIdx(Math.max(i, -1));
                setInput(i >= 0 ? (history[i] ?? "") : "");
              }
            }}
          />
        </div>
        <button className="btn btn-primary" disabled={!running || !input.trim()}>
          Send
        </button>
        {!follow && (
          <button type="button" className="btn" onClick={() => setFollow(true)}>
            ↓ Latest
          </button>
        )}
      </form>
      {error && (
        <p className="text-xs text-danger" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
