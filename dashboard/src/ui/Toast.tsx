import { CheckCircle2, Info, X, XCircle } from "lucide-react";
import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from "react";

import { cn } from "@/lib/cn";

export type ToastKind = "ok" | "error" | "info";

export interface ToastInput {
  kind?: ToastKind;
  title: ReactNode;
  description?: ReactNode;
  durationMs?: number;
}

interface ToastItem extends ToastInput {
  id: number;
}

interface ToastCtx {
  push: (t: ToastInput) => void;
  ok: (title: ReactNode, description?: ReactNode) => void;
  error: (title: ReactNode, description?: ReactNode) => void;
}

const Ctx = createContext<ToastCtx | null>(null);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const seq = useRef(0);
  const dismiss = useCallback((id: number) => setItems((xs) => xs.filter((x) => x.id !== id)), []);
  const push = useCallback(
    (t: ToastInput) => {
      const id = ++seq.current;
      setItems((xs) => [...xs.slice(-4), { ...t, id }]);
      const ms = t.durationMs ?? (t.kind === "error" ? 8000 : 3500);
      setTimeout(() => dismiss(id), ms);
    },
    [dismiss],
  );
  const value = useMemo<ToastCtx>(
    () => ({
      push,
      ok: (title, description) => push({ kind: "ok", title, description }),
      error: (title, description) => push({ kind: "error", title, description }),
    }),
    [push],
  );
  return (
    <Ctx.Provider value={value}>
      {children}
      <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex w-80 flex-col gap-2" aria-live="polite">
        {items.map((t) => (
          <div
            key={t.id}
            role="status"
            className="pointer-events-auto card flex items-start gap-2.5 p-3 animate-fade-in shadow-sm"
          >
            <span className={cn("mt-0.5 shrink-0", t.kind === "error" ? "text-danger" : t.kind === "ok" ? "text-ok" : "text-fg-muted")}>
              {t.kind === "error" ? <XCircle className="size-4" /> : t.kind === "ok" ? <CheckCircle2 className="size-4" /> : <Info className="size-4" />}
            </span>
            <div className="min-w-0 flex-1 text-[13px]">
              <div className="font-medium leading-5">{t.title}</div>
              {t.description ? <div className="text-xs text-fg-muted mt-0.5 break-words">{t.description}</div> : null}
            </div>
            <button type="button" className="text-fg-faint hover:text-fg -m-1 p-1" onClick={() => dismiss(t.id)} aria-label="close">
              <X className="size-3.5" />
            </button>
          </div>
        ))}
      </div>
    </Ctx.Provider>
  );
}

export function useToast(): ToastCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useToast outside ToastProvider");
  return ctx;
}
