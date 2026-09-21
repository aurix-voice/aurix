import { Link } from "@tanstack/react-router";
import { Eraser, Pause, Play, Radio } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import { useEvents, useServerEvents, type LiveEvent } from "@/api/events";
import { useI18n } from "@/i18n";
import { cn } from "@/lib/cn";
import { fmtTime, shortId } from "@/lib/format";
import { Button } from "@/ui/Button";
import { NativeSelect } from "@/ui/Input";
import { Badge, Card, CardHeader, EmptyState, type Tone } from "@/ui/Primitives";

const MAX = 200;

export function eventTone(type: string): Tone {
  if (type.startsWith("quality.alert") || type === "safety.incident" || type === "user.banned" || type === "participant.kicked") return "danger";
  if (type === "quality.recovered" || type === "participant.joined" || type === "channel.created" || type === "channel.activated") return "ok";
  if (type.startsWith("moderation") || type === "safety.risk_changed" || type === "participant.muted") return "warn";
  return "neutral";
}

function str(v: unknown): string | null {
  return typeof v === "string" && v ? v : null;
}

/** Compact one-line summary of an event's payload: who, where, what. */
export function EventSummary({ ev, channelFilter }: { ev: LiveEvent; channelFilter?: string }) {
  const d = ev.envelope?.data ?? {};
  const user = str(d.display_name) ?? str(d.user_id);
  const channel = str(d.channel_id);
  const reason = str(d.reason) ?? str(d.metric) ?? str(d.text) ?? str(d.state) ?? str(d.action);
  const parts: ReactNode[] = [];
  if (user) parts.push(<span key="u" className="font-medium truncate max-w-40">{user.length > 20 ? shortId(user) : user}</span>);
  if (channel && channel !== channelFilter)
    parts.push(
      <Link key="c" to="/live/$channelId" params={{ channelId: channel }} search={{}} className="font-mono text-fg-muted hover:text-fg hover:underline">
        {shortId(channel)}
      </Link>,
    );
  if (reason) parts.push(<span key="r" className="text-fg-muted truncate max-w-56">{reason}</span>);
  if (typeof d.mos === "number") parts.push(<span key="m" className="tabular text-fg-muted">MOS {d.mos.toFixed(2)}</span>);
  return <span className="flex items-center gap-2 min-w-0 text-xs">{parts.length ? parts : <span className="text-fg-faint">—</span>}</span>;
}

/**
 * Live server-event feed for the selected application. `channelId` narrows it to one channel;
 * `types` (a prefix such as `participant.` or an exact type) filters client-side.
 */
export function EventFeed({ channelId, className, height = 420, title, description }: { channelId?: string; className?: string; height?: number; title?: ReactNode; description?: ReactNode }) {
  const { t, locale } = useI18n();
  const { state } = useEvents();
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState("");
  const [items, setItems] = useState<LiveEvent[]>([]);
  const pausedRef = useRef(paused);
  useEffect(() => {
    pausedRef.current = paused;
  }, [paused]);

  useServerEvents(
    useCallback(
      (ev: LiveEvent) => {
        if (pausedRef.current) return;
        if (ev.type === "participant.speaking") return;
        if (channelId && ev.envelope?.data.channel_id !== channelId) return;
        setItems((prev) => {
          const next = [ev, ...prev];
          return next.length > MAX ? next.slice(0, MAX) : next;
        });
      },
      [channelId],
    ),
    [channelId],
  );

  const heads = useMemo(() => {
    const s = new Set<string>();
    for (const it of items) s.add(it.type.split(".")[0] ?? it.type);
    return [...s].sort();
  }, [items]);
  const visible = filter ? items.filter((i) => i.type.startsWith(filter)) : items;

  return (
    <Card className={cn("flex flex-col", className)}>
      <CardHeader
        title={
          <span className="inline-flex items-center gap-2">
            <Radio className={cn("size-3.5", state === "live" ? "text-ok" : "text-fg-faint")} />
            {title ?? t("live.events")}
          </span>
        }
        description={description ?? t("live.events.desc")}
        actions={
          <>
            <NativeSelect value={filter} onChange={(e) => setFilter(e.target.value)} className="h-7 text-xs w-32" aria-label={t("live.events.filter")}>
              <option value="">{t("common.all")}</option>
              {heads.map((h) => (
                <option key={h} value={h + "."}>
                  {h}
                </option>
              ))}
            </NativeSelect>
            <Button variant="ghost" size="icon" className="h-7 w-7" onClick={() => setPaused((p) => !p)} aria-label={paused ? t("live.events.resume") : t("live.events.pause")}>
              {paused ? <Play className="size-3.5" /> : <Pause className="size-3.5" />}
            </Button>
            <Button variant="ghost" size="icon" className="h-7 w-7" onClick={() => setItems([])} aria-label={t("live.events.clear")}>
              <Eraser className="size-3.5" />
            </Button>
          </>
        }
      />
      {paused ? (
        <div className="px-4 pb-2">
          <Badge tone="warn">{t("live.events.paused")}</Badge>
        </div>
      ) : null}
      <div className="overflow-y-auto subtle-scroll border-t border-border" style={{ maxHeight: height }}>
        {visible.length === 0 ? (
          <EmptyState compact title={state === "live" ? t("live.events.waiting") : t("live.events.offline")} />
        ) : (
          <ul className="divide-y divide-border">
            {visible.map((ev, i) => (
              <li key={ev.id ?? `${ev.receivedAt}-${i}`} className="px-4 py-2 flex items-start gap-3">
                <span className="tabular text-[11px] text-fg-faint pt-0.5 shrink-0">{fmtTime(locale, ev.receivedAt)}</span>
                <div className="min-w-0 flex-1 flex flex-col gap-1">
                  <Badge tone={eventTone(ev.type)} className="self-start font-mono">
                    {ev.type}
                  </Badge>
                  <EventSummary ev={ev} channelFilter={channelId} />
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>
    </Card>
  );
}
