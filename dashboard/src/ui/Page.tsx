import { AlertTriangle, PowerOff, RefreshCw } from "lucide-react";
import type { ReactNode } from "react";

import { errorCode, errorMessage } from "@/api/client";
import { useI18n } from "@/i18n";
import { cn } from "@/lib/cn";

import { Button } from "./Button";
import { Callout, EmptyState, Spinner } from "./Primitives";

export function PageHeader({ title, description, actions, tabs, className }: { title: ReactNode; description?: ReactNode; actions?: ReactNode; tabs?: ReactNode; className?: string }) {
  return (
    <div className={cn("flex flex-col gap-3 mb-5", className)}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h1 className="text-lg font-semibold tracking-tight leading-7">{title}</h1>
          {description ? <p className="text-[13px] text-fg-muted mt-0.5 max-w-2xl">{description}</p> : null}
        </div>
        {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
      </div>
      {tabs}
    </div>
  );
}

export function Toolbar({ children, className, end }: { children?: ReactNode; className?: string; end?: ReactNode }) {
  return (
    <div className={cn("flex flex-wrap items-center gap-2 px-3 py-2 border-b border-border", className)}>
      {children}
      {end ? <div className="ml-auto flex items-center gap-2">{end}</div> : null}
    </div>
  );
}

export function QueryError({ error, onRetry, compact }: { error: unknown; onRetry?: () => void; compact?: boolean }) {
  const { t, locale } = useI18n();
  const msg = errorMessage(error, locale);
  if (errorCode(error) === "INVALID_CONFIG") {
    return <EmptyState compact={compact} icon={<PowerOff className="size-6" strokeWidth={1.5} />} title={t("error.featureDisabled")} description={msg} />;
  }
  if (compact) {
    return (
      <Callout
        tone="danger"
        title={t("common.error")}
        action={
          onRetry ? (
            <Button variant="ghost" size="sm" onClick={onRetry}>
              <RefreshCw className="size-3.5" /> {t("common.retry")}
            </Button>
          ) : null
        }
      >
        {msg}
      </Callout>
    );
  }
  return (
    <EmptyState
      icon={<AlertTriangle className="size-6 text-danger" strokeWidth={1.5} />}
      title={t("common.error")}
      description={msg}
      action={
        onRetry ? (
          <Button size="sm" onClick={onRetry}>
            <RefreshCw className="size-3.5" /> {t("common.retry")}
          </Button>
        ) : null
      }
    />
  );
}

export function Loading({ className }: { className?: string }) {
  return (
    <div className={cn("flex items-center justify-center py-16", className)}>
      <Spinner />
    </div>
  );
}

/** Two-column detail layout: main content + sticky side panel on wide screens. */
export function SplitLayout({ main, side, className }: { main: ReactNode; side: ReactNode; className?: string }) {
  return (
    <div className={cn("grid gap-4 lg:grid-cols-[minmax(0,1fr)_320px] items-start", className)}>
      <div className="min-w-0 flex flex-col gap-4">{main}</div>
      <div className="flex flex-col gap-4 lg:sticky lg:top-4">{side}</div>
    </div>
  );
}

export function Section({ title, children, actions, className }: { title?: ReactNode; children: ReactNode; actions?: ReactNode; className?: string }) {
  return (
    <section className={cn("flex flex-col gap-2.5", className)}>
      {title || actions ? (
        <div className="flex items-center justify-between gap-2">
          {title ? <h2 className="text-[13px] font-semibold text-fg-muted">{title}</h2> : <span />}
          {actions}
        </div>
      ) : null}
      {children}
    </section>
  );
}
