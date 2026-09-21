import { Link, useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";

import type { T } from "@/api/client";
import { useI18n } from "@/i18n";
import type { MessageKey } from "@/i18n";
import { shortId } from "@/lib/format";
import { moderationRoute } from "@/router";
import { Badge, CopyButton, Mono, type Tone } from "@/ui/Primitives";

export { asNumber, asRecord, asString, asStringArray, isSafetyType } from "./model";

export type ModTab = "events" | "incidents" | "bans" | "users" | "chat";
export const MOD_TABS: readonly ModTab[] = ["events", "incidents", "bans", "users", "chat"];

export interface ModSearch {
  tab?: string;
  user?: string;
  channel?: string;
  event?: string;
}

/** Route-state accessor for `/moderation`: tab + selected user/channel/event live in the URL. */
export function useModSearch(): {
  tab: ModTab;
  user: string | null;
  channel: string | null;
  event: string | null;
  go: (patch: Partial<{ tab: ModTab; user: string | null; channel: string | null; event: string | null }>, replace?: boolean) => void;
} {
  const search = moderationRoute.useSearch();
  const navigate = useNavigate();
  const tab: ModTab = MOD_TABS.includes(search.tab as ModTab) ? (search.tab as ModTab) : "events";
  const cur = { tab, user: search.user ?? null, channel: search.channel ?? null, event: search.event ?? null };
  const go = (patch: Partial<typeof cur>, replace = true) => {
    const next = { ...cur, ...patch };
    void navigate({
      to: "/moderation",
      search: {
        tab: next.tab === "events" ? undefined : next.tab,
        user: next.user ?? undefined,
        channel: next.channel ?? undefined,
        event: next.event ?? undefined,
      },
      replace,
    });
  };
  return { ...cur, go };
}

export const EVENT_STATUSES = ["pending", "resolved"] as const;

export function statusTone(status: string): Tone {
  if (status === "pending" || status === "open") return "warn";
  if (status === "resolved") return "ok";
  if (status === "dismissed") return "neutral";
  return "neutral";
}

export function StatusBadge({ status }: { status: string }) {
  const { t } = useI18n();
  const key = `moderation.status.${status}`;
  const label = key === "moderation.status.pending" || key === "moderation.status.resolved" || key === "moderation.status.dismissed" ? t(key) : status;
  return (
    <Badge tone={statusTone(status)} dot>
      {label}
    </Badge>
  );
}

export function eventTypeTone(type: string): Tone {
  if (type === "ban" || type === "safety.voice" || type === "safety.text") return "danger";
  if (type === "kick" || type === "mute") return "warn";
  if (type === "report" || type === "user_report") return "accent";
  return "neutral";
}

const TYPE_KEYS: Record<string, MessageKey> = {
  report: "moderation.type.report",
  user_report: "moderation.type.user_report",
  kick: "moderation.type.kick",
  mute: "moderation.type.mute",
  ban: "moderation.type.ban",
  "safety.voice": "moderation.type.safety_voice",
  "safety.text": "moderation.type.safety_text",
};

export function useEventTypeLabel(): (type: string) => string {
  const { t } = useI18n();
  return (type) => {
    const key = TYPE_KEYS[type];
    return key ? t(key) : type;
  };
}

export function EventTypeBadge({ type }: { type: string }) {
  const label = useEventTypeLabel();
  return <Badge tone={eventTypeTone(type)}>{label(type)}</Badge>;
}

export function riskTone(level: T.RiskLevel): Tone {
  switch (level) {
    case "high":
      return "danger";
    case "elevated":
      return "warn";
    case "low":
      return "neutral";
    default:
      return "ok";
  }
}

export function RiskBadge({ risk }: { risk: T.SafetyRisk }) {
  const { t } = useI18n();
  return (
    <Badge tone={riskTone(risk.risk_level)} dot>
      {t(`moderation.risk.${risk.risk_level}`)} · {risk.risk_score.toFixed(2)}
    </Badge>
  );
}

/** Short user id that opens the user in the Users tab; keeps the copy affordance. */
export function UserRef({ id, label, className }: { id: string | null | undefined; label?: ReactNode; className?: string }) {
  if (!id) return <span className="text-fg-faint">—</span>;
  return (
    <span className={className ?? "inline-flex items-center gap-1"}>
      <Link to="/moderation" search={{ tab: "users", user: id }} className="hover:underline underline-offset-2" title={id}>
        {label ?? <Mono>{shortId(id)}</Mono>}
      </Link>
      <CopyButton value={id} />
    </span>
  );
}

export function ChannelRef({ id }: { id: string | null | undefined }) {
  if (!id) return <span className="text-fg-faint">—</span>;
  return (
    <span className="inline-flex items-center gap-1">
      <Link to="/live/$channelId" params={{ channelId: id }} className="hover:underline underline-offset-2" title={id}>
        <Mono>{shortId(id)}</Mono>
      </Link>
      <CopyButton value={id} />
    </span>
  );
}

