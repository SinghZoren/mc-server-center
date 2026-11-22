import { createPortal } from "react-dom";
import { useEffect, useId, useRef, type ReactNode } from "react";
import type { InstanceState } from "../lib/api";
import { stateLabel, stateTone } from "../lib/text";

const toneColor = { live: "var(--color-moss)", busy: "var(--color-amber)", off: "var(--color-faint)", bad: "var(--color-danger)" };

export function StatusPill({ state, large = false }: { state: InstanceState; large?: boolean }) {
  const tone = stateTone(state);
  return (
    <span
      role="status"
      aria-label={`Server status: ${stateLabel[state]}`}
      className={`inline-flex items-center gap-2 ${large ? "text-sm" : "text-xs"} font-semibold`}
      style={{ color: tone === "off" ? "var(--color-muted)" : toneColor[tone] }}
    >
      <span className={`status-dot ${tone === "live" ? "live" : ""}`} style={{ background: toneColor[tone] }} />
      {stateLabel[state]}
    </span>
  );
}

export function Spinner({ label = "Working" }: { label?: string }) {
  return (
    <span role="progressbar" aria-label={label} className="inline-flex gap-1 align-middle">
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="inline-block h-2 w-2 bg-ember"
          style={{ animation: `pulse-block 900ms ${i * 150}ms ease-in-out infinite` }}
        />
      ))}
      <style>{`@keyframes pulse-block { 0%,100% { opacity: .25 } 50% { opacity: 1 } }`}</style>
    </span>
  );
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: ReactNode;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div>
      <label htmlFor={id} className="label">
        {label}
      </label>
      {children(id)}
      {hint && <p className="mt-1.5 text-xs text-muted">{hint}</p>}
    </div>
  );
}

export function Notice({ tone = "info", children }: { tone?: "info" | "warn" | "error" | "ok"; children: ReactNode }) {
  const color = { info: "var(--color-muted)", warn: "var(--color-amber)", error: "var(--color-danger)", ok: "var(--color-moss)" }[tone];
  return (
    <div role={tone === "error" ? "alert" : "note"} className="border-l-2 bg-panel-2/60 px-3 py-2 text-sm" style={{ borderColor: color }}>
      {children}
    </div>
  );
}

/** Accessible modal: focus moves in, Escape closes, focus returns to the opener. */
export function Modal({
  title,
  onClose,
  children,
  width = 520,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  width?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    const first = ref.current?.querySelector<HTMLElement>("input, select, textarea, button");
    first?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      opener?.focus();
    };
  }, [onClose]);
  // Rendered into <body> so cards with animations/transforms can't trap or cover it.
  return createPortal(
    <div className="fixed inset-0 z-40 grid place-items-center bg-black/60 p-6" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div ref={ref} role="dialog" aria-modal="true" aria-label={title} className="block-card rise max-h-[90vh] w-full overflow-y-auto p-6" style={{ maxWidth: width }}>
        <h2 className="font-display mb-4 text-xl font-semibold">{title}</h2>
        {children}
      </div>
    </div>,
    document.body,
  );
}

export function Confirm({
  title,
  body,
  confirmLabel,
  danger = false,
  onConfirm,
  onClose,
  children,
}: {
  title: string;
  body: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void;
  onClose: () => void;
  children?: ReactNode;
}) {
  return (
    <Modal title={title} onClose={onClose}>
      <div className="space-y-4 text-sm text-ink/90">{body}</div>
      {children && <div className="mt-4">{children}</div>}
      <div className="mt-6 flex justify-end gap-3">
        <button className="btn" onClick={onClose}>
          Cancel
        </button>
        <button className={`btn ${danger ? "btn-danger" : "btn-primary"}`} onClick={onConfirm}>
          {confirmLabel}
        </button>
      </div>
    </Modal>
  );
}

export function EmptyState({ title, children, action }: { title: string; children: ReactNode; action?: ReactNode }) {
  return (
    <div className="rise mx-auto flex max-w-md flex-col items-start gap-3 py-16">
      <div className="grid grid-cols-3 gap-1 opacity-80" aria-hidden>
        {Array.from({ length: 9 }, (_, i) => (
          <span key={i} className="h-3 w-3" style={{ background: i % 4 === 0 ? "var(--color-ember)" : "var(--color-line)" }} />
        ))}
      </div>
      <h2 className="font-display text-2xl font-semibold">{title}</h2>
      <div className="text-muted">{children}</div>
      {action}
    </div>
  );
}
