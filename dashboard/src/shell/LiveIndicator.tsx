import { useEvents } from "@/api/events";
import { useAppScope } from "@/api/scope";
import { useI18n } from "@/i18n";
import { fmtRelative } from "@/lib/format";
import { cn } from "@/lib/cn";
import { Tip } from "@/ui/Primitives";

export function LiveIndicator() {
  const { t, locale } = useI18n();
  const { appId } = useAppScope();
  const { state, lastEventAt, lagged } = useEvents();
  if (!appId || state === "idle") return null;

  const label =
    state === "live"
      ? t("common.live")
      : state === "unauthorized"
        ? t("common.offline")
        : t("common.reconnecting");
  const dot =
    state === "live" ? "bg-ok" : state === "unauthorized" ? "bg-danger" : "bg-warn animate-pulse";

  const detail = [
    lastEventAt ? `${t("events.lastEvent")}: ${fmtRelative(locale, new Date(lastEventAt).toISOString())}` : t("events.noEventsYet"),
    lagged > 0 ? t("events.lagged", { n: lagged }) : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <Tip content={detail} side="bottom">
      <div
        className="hidden sm:inline-flex items-center gap-1.5 h-7 px-2 rounded-md text-[12px] text-fg-muted select-none"
        role="status"
        aria-live="polite"
      >
        <span className={cn("size-1.5 rounded-full", dot)} aria-hidden />
        {label}
      </div>
    </Tip>
  );
}
