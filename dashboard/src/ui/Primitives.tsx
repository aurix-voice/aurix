import * as SwitchPrimitive from "@radix-ui/react-switch";
import * as TooltipPrimitive from "@radix-ui/react-tooltip";
import { Check, Copy, Inbox } from "lucide-react";
import { useState, type HTMLAttributes, type ReactNode } from "react";

import { useT } from "@/i18n";
import { cn } from "@/lib/cn";

import { Button } from "./Button";

// ---- Card
export function Card({ className, ...rest }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("card", className)} {...rest} />;
}

export function CardHeader({
  title,
  description,
  actions,
  className,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex items-start justify-between gap-3 px-4 pt-3.5 pb-3", className)}>
      <div className="min-w-0">
        <h3 className="text-[13px] font-semibold leading-5">{title}</h3>
        {description ? <p className="text-xs text-fg-muted mt-0.5">{description}</p> : null}
      </div>
      {actions ? <div className="flex items-center gap-1.5 shrink-0">{actions}</div> : null}
    </div>
  );
}

// ---- Badge
export type Tone = "neutral" | "ok" | "warn" | "danger" | "accent";
const tones: Record<Tone, string> = {
  neutral: "bg-surface-2 text-fg-muted border-border",
  ok: "bg-ok-soft text-ok border-transparent",
  warn: "bg-warn-soft text-warn border-transparent",
  danger: "bg-danger-soft text-danger border-transparent",
  accent: "bg-accent-soft text-accent border-transparent",
};
export function Badge({ tone = "neutral", className, dot, children }: { tone?: Tone; className?: string; dot?: boolean; children: ReactNode }) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 h-5 px-1.5 rounded-md border text-[11px] font-medium whitespace-nowrap leading-none",
        tones[tone],
        className,
      )}
    >
      {dot ? <span className="size-1.5 rounded-full bg-current" aria-hidden /> : null}
      {children}
    </span>
  );
}

// ---- Switch
export function Switch({
  checked,
  onCheckedChange,
  disabled,
  label,
  id,
}: {
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  disabled?: boolean;
  label?: ReactNode;
  id?: string;
}) {
  return (
    <label className="inline-flex items-center gap-2 text-[13px] select-none">
      <SwitchPrimitive.Root
        id={id}
        checked={checked}
        onCheckedChange={onCheckedChange}
        disabled={disabled}
        className="relative h-[18px] w-[30px] rounded-full bg-border-strong data-[state=checked]:bg-fg transition-colors disabled:opacity-50 outline-none"
      >
        <SwitchPrimitive.Thumb className="block size-3.5 translate-x-0.5 rounded-full bg-white transition-transform data-[state=checked]:translate-x-[14px]" />
      </SwitchPrimitive.Root>
      {label}
    </label>
  );
}

// ---- Tooltip
export function Tip({ content, children, side = "top" }: { content: ReactNode; children: ReactNode; side?: "top" | "bottom" | "left" | "right" }) {
  if (!content) return <>{children}</>;
  return (
    <TooltipPrimitive.Root delayDuration={250}>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal>
        <TooltipPrimitive.Content
          side={side}
          sideOffset={4}
          className="z-50 max-w-xs rounded-md border border-border bg-surface px-2 py-1 text-xs text-fg animate-fade-in"
        >
          {content}
        </TooltipPrimitive.Content>
      </TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

export const TooltipProvider = TooltipPrimitive.Provider;

// ---- Copy
export function CopyButton({ value, className, size = "icon" }: { value: string; className?: string; size?: "icon" | "sm" }) {
  const t = useT();
  const [done, setDone] = useState(false);
  return (
    <Tip content={done ? t("common.copied") : t("common.copy")}>
      <Button
        variant="ghost"
        size={size}
        className={cn(size === "icon" && "h-6 w-6", className)}
        aria-label={t("common.copy")}
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(value);
            setDone(true);
            setTimeout(() => setDone(false), 1200);
          } catch {
            /* clipboard blocked */
          }
        }}
      >
        {done ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
        {size === "sm" ? (done ? t("common.copied") : t("common.copy")) : null}
      </Button>
    </Tip>
  );
}

// ---- Mono / code
export function Mono({ children, className, title }: { children: ReactNode; className?: string; title?: string }) {
  return (
    <span title={title} className={cn("font-mono text-[12px] tabular", className)}>
      {children}
    </span>
  );
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Compact id: UUIDs keep the time prefix and the random tail (v7 ids minted together share the prefix). */
export function shortId(id: string, short = 8): string {
  if (id.length <= short) return id;
  if (UUID_RE.test(id)) return `${id.slice(0, short)}…${id.slice(-4)}`;
  return id.slice(0, short);
}

export function IdChip({ id, short = 8 }: { id: string; short?: number }) {
  return (
    <span className="inline-flex items-center gap-0.5 group whitespace-nowrap">
      <Mono title={id} className="text-fg-muted">
        {shortId(id, short)}
      </Mono>
      <CopyButton value={id} className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100" />
    </span>
  );
}

export function CodeBlock({ value, className, maxHeight = "24rem" }: { value: string; className?: string; maxHeight?: string }) {
  return (
    <div className={cn("relative group", className)}>
      <pre
        className="font-mono text-[12px] leading-relaxed bg-surface-2 border border-border rounded-md p-3 overflow-auto subtle-scroll whitespace-pre-wrap break-all"
        style={{ maxHeight }}
      >
        {value}
      </pre>
      <CopyButton value={value} className="absolute top-1.5 right-1.5 bg-surface border border-border opacity-0 group-hover:opacity-100" />
    </div>
  );
}

// ---- Empty / loading / error states
export function EmptyState({ title, description, icon, action, compact }: { title: ReactNode; description?: ReactNode; icon?: ReactNode; action?: ReactNode; compact?: boolean }) {
  return (
    <div className={cn("flex flex-col items-center justify-center text-center gap-1.5", compact ? "py-8 px-4" : "py-16 px-6")}>
      <div className="text-fg-faint mb-1">{icon ?? <Inbox className="size-6" strokeWidth={1.5} />}</div>
      <div className="text-[13px] font-medium">{title}</div>
      {description ? <div className="text-xs text-fg-muted max-w-sm">{description}</div> : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("animate-pulse rounded-md bg-surface-2", className)} aria-hidden />;
}

export function Spinner({ className }: { className?: string }) {
  return (
    <span
      className={cn("inline-block size-3.5 rounded-full border-2 border-border-strong border-t-fg animate-spin", className)}
      role="status"
      aria-label="loading"
    />
  );
}

// ---- Key/value list
export function KV({ items, className, cols = 2 }: { items: Array<{ k: ReactNode; v: ReactNode; hidden?: boolean }>; className?: string; cols?: 1 | 2 | 3 | 4 }) {
  const grid = { 1: "grid-cols-1", 2: "grid-cols-2", 3: "grid-cols-3", 4: "grid-cols-4" }[cols];
  return (
    <dl className={cn("grid gap-x-6 gap-y-2.5", grid, className)}>
      {items
        .filter((i) => !i.hidden)
        .map((i, idx) => (
          <div key={idx} className="min-w-0">
            <dt className="text-[11px] uppercase tracking-wide text-fg-faint font-medium">{i.k}</dt>
            <dd className="text-[13px] mt-0.5 truncate">{i.v ?? "—"}</dd>
          </div>
        ))}
    </dl>
  );
}

// ---- Stat
export function Stat({ label, value, hint, tone, trend }: { label: ReactNode; value: ReactNode; hint?: ReactNode; tone?: Tone; trend?: ReactNode }) {
  return (
    <Card className="px-4 py-3.5">
      <div className="text-xs text-fg-muted">{label}</div>
      <div className={cn("mt-1 text-2xl font-semibold tabular tracking-tight", tone === "danger" && "text-danger", tone === "warn" && "text-warn", tone === "ok" && "text-ok")}>
        {value}
      </div>
      {(hint || trend) && (
        <div className="mt-1 flex items-center gap-2 text-xs text-fg-faint">
          {trend}
          <span className="truncate">{hint}</span>
        </div>
      )}
    </Card>
  );
}

// ---- Progress
export function Progress({ value, max, tone, className }: { value: number; max: number | null | undefined; tone?: Tone; className?: string }) {
  const pct = !max || max <= 0 ? 0 : Math.min(100, (value / max) * 100);
  const color = tone === "danger" ? "bg-danger" : tone === "warn" ? "bg-warn" : tone === "ok" ? "bg-ok" : "bg-fg";
  return (
    <div className={cn("h-1.5 w-full rounded-full bg-surface-2 overflow-hidden", className)} role="progressbar" aria-valuenow={value} aria-valuemax={max ?? undefined}>
      <div className={cn("h-full rounded-full transition-[width]", color)} style={{ width: `${pct}%` }} />
    </div>
  );
}

// ---- Field
export function Field({ label, hint, error, children, htmlFor, required, className }: { label: ReactNode; hint?: ReactNode; error?: ReactNode; children: ReactNode; htmlFor?: string; required?: boolean; className?: string }) {
  return (
    <div className={cn("flex flex-col gap-1.5", className)}>
      <label htmlFor={htmlFor} className="text-xs font-medium text-fg-muted">
        {label}
        {required ? <span className="text-danger"> *</span> : null}
      </label>
      {children}
      {error ? <p className="text-xs text-danger">{error}</p> : hint ? <p className="text-xs text-fg-faint">{hint}</p> : null}
    </div>
  );
}

// ---- Alert / callout
export function Callout({ tone = "neutral", title, children, className, action }: { tone?: Tone; title?: ReactNode; children?: ReactNode; className?: string; action?: ReactNode }) {
  const style: Record<Tone, string> = {
    neutral: "bg-surface-2 border-border",
    ok: "bg-ok-soft border-transparent",
    warn: "bg-warn-soft border-transparent",
    danger: "bg-danger-soft border-transparent",
    accent: "bg-accent-soft border-transparent",
  };
  return (
    <div className={cn("rounded-md border px-3 py-2.5 text-[13px] flex items-start gap-3", style[tone], className)}>
      <div className="min-w-0 flex-1">
        {title ? <div className="font-medium">{title}</div> : null}
        {children ? <div className={cn("text-fg-muted", title && "mt-0.5")}>{children}</div> : null}
      </div>
      {action}
    </div>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="inline-flex h-5 items-center rounded border border-border bg-surface px-1.5 font-mono text-[10px] text-fg-muted">{children}</kbd>;
}
