import { useNavigate } from "@tanstack/react-router";

import type { T } from "@/api/client";
import { useI18n } from "@/i18n";
import { recordingsRoute } from "@/router";
import { Badge } from "@/ui/Primitives";

import { statusTone, transcriptTone } from "./model";

export function useRecordingsSearch(): {
  id: string | null;
  channel: string | null;
  go: (patch: Partial<{ id: string | null; channel: string | null }>, replace?: boolean) => void;
} {
  const search = recordingsRoute.useSearch();
  const navigate = useNavigate();
  const cur = { id: search.id ?? null, channel: search.channel ?? null };
  const go = (patch: Partial<typeof cur>, replace = true) => {
    const next = { ...cur, ...patch };
    void navigate({ to: "/recordings", search: { id: next.id ?? undefined, channel: next.channel ?? undefined }, replace });
  };
  return { ...cur, go };
}

export function RecordingStatusBadge({ status }: { status: T.RecordingStatus }) {
  const { t } = useI18n();
  return (
    <Badge tone={statusTone(status)} dot={status === "recording" || status === "processing"}>
      {t(`recordings.status.${status}`)}
    </Badge>
  );
}

export function RecordingKindBadge({ kind }: { kind: T.RecordingKind }) {
  const { t } = useI18n();
  return <Badge tone={kind === "evidence" ? "warn" : kind === "mixdown" ? "accent" : "neutral"}>{t(`recordings.kind.${kind}`)}</Badge>;
}

export function TranscriptStatusBadge({ status }: { status: T.RecordingTranscriptStatus }) {
  const { t } = useI18n();
  return (
    <Badge tone={transcriptTone(status)} dot={status === "running"}>
      {t(`recordings.transcript.status.${status}`)}
    </Badge>
  );
}
