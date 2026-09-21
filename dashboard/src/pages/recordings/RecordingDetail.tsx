import { Download, FileText, Square, Trash2, X } from "lucide-react";
import { useMemo, useState } from "react";

import { downloadRaw, errorMessage, type T } from "@/api/client";
import { useApi, useDeleteRecordingMutation, useRecordingQuery, useStopRecordingMutation, useTranscribeMutation, useTranscriptQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtDateTime, fmtDuration, fmtPercent, shortId } from "@/lib/format";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { QueryError } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, CopyButton, KV, Mono, Skeleton } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { ChannelRef, UserRef } from "../moderation/shared";
import { fileExt, fmtOffset, isInFlight, speakerIndex, transcriptInFlight, TRANSCRIPT_FORMATS } from "./model";
import { RecordingKindBadge, RecordingStatusBadge, TranscriptStatusBadge } from "./shared";

const SPEAKER_TONES = ["accent", "ok", "warn", "danger", "neutral"] as const;

function Transcript({ tr }: { tr: T.RecordingTranscript }) {
  const { t, locale } = useI18n();
  const speakers = useMemo(() => speakerIndex(tr.segments), [tr.segments]);
  if (tr.status === "failed") return <Callout tone="danger" title={t("recordings.transcript.failed")}>{tr.error ?? "—"}</Callout>;
  if (tr.status !== "ready") return <Callout tone="neutral">{t("recordings.transcript.pending")}</Callout>;
  if (tr.segments.length === 0) return <p className="text-[13px] text-fg-muted">{tr.text.trim() || t("recordings.transcript.empty")}</p>;
  return (
    <ol className="flex flex-col gap-1.5 max-h-[28rem] overflow-y-auto subtle-scroll pr-1">
      {tr.segments.map((s, i) => {
        const key = s.speaker ?? "?";
        const idx = speakers.get(key) ?? 0;
        return (
          <li key={i} className="grid grid-cols-[4.5rem_minmax(0,1fr)] gap-2 text-[13px]">
            <span className="tabular text-fg-faint text-xs pt-0.5" title={`${fmtOffset(s.start_ms)} – ${fmtOffset(s.end_ms)}`}>
              {fmtOffset(s.start_ms)}
            </span>
            <div className="min-w-0">
              <div className="flex items-center gap-1.5 flex-wrap">
                {speakers.size > 1 || s.speaker ? (
                  <Badge tone={SPEAKER_TONES[idx % SPEAKER_TONES.length]}>{s.speaker ? shortId(s.speaker) : t("recordings.speaker.unknown")}</Badge>
                ) : null}
                {s.language && s.language !== tr.language ? <span className="text-[11px] uppercase text-fg-faint">{s.language}</span> : null}
                {s.confidence < 0.6 ? (
                  <span className="text-[11px] text-warn" title={t("recordings.transcript.confidence")}>
                    {fmtPercent(locale, s.confidence * 100, 0)}
                  </span>
                ) : null}
              </div>
              <p className="whitespace-pre-wrap break-words">{s.text}</p>
            </div>
          </li>
        );
      })}
    </ol>
  );
}

export function RecordingDetail({ id, onClose, onDeleted }: { id: string; onClose: () => void; onDeleted: () => void }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const api = useApi();
  const toast = useToast();
  const rec = useRecordingQuery(id, (d) => (d && isInFlight(d.recording) ? 5_000 : false));
  const tr = useTranscriptQuery(id, (d) => (transcriptInFlight(d) ? 5_000 : false));
  const stop = useStopRecordingMutation();
  const del = useDeleteRecordingMutation();
  const transcribe = useTranscribeMutation();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState<"file" | T.GetRecordingTranscriptFormat | null>(null);
  const canWrite = canApp("recordings:write");

  const r = rec.data?.recording;
  const failOnce = (e: unknown) => toast.error(errorMessage(e, locale));

  const downloadFile = async () => {
    if (!r) return;
    setBusy("file");
    try {
      if (rec.data?.download_url) {
        window.open(rec.data.download_url, "_blank", "noopener");
      } else {
        downloadRaw(await api.downloadRecordingRaw(r.id), `${r.kind}-${shortId(r.id)}.${fileExt(r.format)}`);
      }
    } catch (e) {
      failOnce(e);
    } finally {
      setBusy(null);
    }
  };
  const downloadTranscript = async (format: T.GetRecordingTranscriptFormat) => {
    if (!r) return;
    setBusy(format);
    try {
      downloadRaw(await api.getRecordingTranscriptRaw(r.id, { format }), `transcript-${shortId(r.id)}.${format}`);
    } catch (e) {
      failOnce(e);
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-2">
              {r ? <RecordingKindBadge kind={r.kind} /> : null}
              <Mono>{shortId(id, 12)}</Mono>
              <CopyButton value={id} />
            </span>
          }
          actions={
            <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
              <X className="size-4" />
            </Button>
          }
        />
        <div className="px-4 pb-4 flex flex-col gap-4">
          {rec.isError ? (
            <QueryError error={rec.error} onRetry={() => void rec.refetch()} compact />
          ) : !r ? (
            <Skeleton className="h-40" />
          ) : (
            <>
              <div className="flex items-center gap-2 flex-wrap">
                <RecordingStatusBadge status={r.status} />
                {r.encrypted ? <Badge tone="neutral">{t("recordings.encrypted")}</Badge> : null}
                {rec.data?.consent ? <Badge tone={rec.data.consent === "accepted" ? "ok" : rec.data.consent === "declined" ? "danger" : "warn"}>{t(`recordings.consent.${rec.data.consent}`)}</Badge> : null}
                <span className="text-xs text-fg-faint">{r.format === "wav" ? "WAV" : "Ogg/Opus"}</span>
              </div>
              {r.error ? <Callout tone="danger" title={t("recordings.error")}>{r.error}</Callout> : null}
              <KV
                cols={2}
                items={[
                  { k: t("common.channel"), v: <ChannelRef id={r.channel_id} /> },
                  { k: t("common.user"), v: <UserRef id={r.user_id} />, hidden: r.kind === "mixdown" },
                  { k: t("common.session"), v: <Mono title={r.session_id}>{shortId(r.session_id)}</Mono>, hidden: r.kind === "mixdown" },
                  { k: t("recordings.duration"), v: fmtDuration(locale, r.duration_secs) },
                  { k: t("recordings.size"), v: fmtBytes(locale, r.file_size_bytes) },
                  { k: t("recordings.started"), v: fmtDateTime(locale, r.started_at) },
                  { k: t("recordings.ended"), v: r.ended_at ? fmtDateTime(locale, r.ended_at) : "—" },
                  { k: t("recordings.firstAudio"), v: r.audio_started_at ? fmtDateTime(locale, r.audio_started_at) : "—" },
                  { k: t("recordings.expires"), v: fmtDateTime(locale, r.expires_at) },
                  { k: t("common.node"), v: r.node_id ? <Mono>{r.node_id}</Mono> : "—" },
                ]}
              />
              {r.sources && r.sources.length > 0 ? (
                <div>
                  <div className="text-[11px] uppercase tracking-wide text-fg-faint font-medium mb-1">{t("recordings.sources")}</div>
                  <div className="flex flex-wrap gap-1.5">
                    {r.sources.map((s) => (
                      <Mono key={s} title={s}>
                        {shortId(s)}
                      </Mono>
                    ))}
                  </div>
                </div>
              ) : null}
              <div className="flex flex-wrap gap-2">
                <Button variant="secondary" size="sm" onClick={() => void downloadFile()} disabled={r.status !== "ready"} loading={busy === "file"} title={r.status !== "ready" ? t("recordings.notReady") : undefined}>
                  <Download className="size-3.5" />
                  {t("common.download")}
                </Button>
                {canWrite && r.status === "recording" ? (
                  <Button variant="secondary" size="sm" loading={stop.isPending} onClick={() => stop.mutateAsync({ recordingId: r.id }).then(() => toast.ok(t("recordings.stopped")), failOnce)}>
                    <Square className="size-3.5" />
                    {t("recordings.stop")}
                  </Button>
                ) : null}
                {canWrite && r.status === "ready" && !transcriptInFlight(tr.data) ? (
                  <Button
                    variant="secondary"
                    size="sm"
                    loading={transcribe.isPending}
                    onClick={() => transcribe.mutateAsync({ recordingId: r.id }).then(() => toast.ok(t("recordings.transcribe.started")), failOnce)}
                  >
                    <FileText className="size-3.5" />
                    {tr.data ? t("recordings.transcribe.again") : t("recordings.transcribe")}
                  </Button>
                ) : null}
                {canWrite && r.status !== "recording" ? (
                  <Button variant="danger" size="sm" className="ml-auto" onClick={() => setConfirmDelete(true)}>
                    <Trash2 className="size-3.5" />
                    {t("common.delete")}
                  </Button>
                ) : null}
              </div>
            </>
          )}
        </div>
      </Card>

      {r && r.status === "ready" ? (
        <Card>
          <CardHeader
            title={t("recordings.transcript")}
            description={t("recordings.transcribe.desc")}
            actions={
              tr.data ? (
                <span className="inline-flex items-center gap-1.5">
                  <TranscriptStatusBadge status={tr.data.status} />
                  {tr.data.status === "ready"
                    ? TRANSCRIPT_FORMATS.map((f) => (
                        <Button key={f} variant="ghost" size="xs" loading={busy === f} onClick={() => void downloadTranscript(f)}>
                          .{f}
                        </Button>
                      ))
                    : null}
                </span>
              ) : null
            }
          />
          <div className="px-4 pb-4">
            {tr.isError ? (
              <QueryError error={tr.error} onRetry={() => void tr.refetch()} compact />
            ) : tr.isPending ? (
              <Skeleton className="h-16" />
            ) : tr.data === null ? (
              <p className="text-[13px] text-fg-muted">{t("recordings.transcript.none")}</p>
            ) : (
              <>
                <Transcript tr={tr.data} />
                <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1 text-xs text-fg-faint">
                  {tr.data.provider ? <span>{tr.data.provider}</span> : null}
                  {tr.data.language ? <span className="uppercase">{tr.data.language}</span> : null}
                  <span>{fmtOffset(tr.data.duration_ms)}</span>
                  <span>{fmtDateTime(locale, tr.data.finished_at ?? tr.data.requested_at)}</span>
                </div>
              </>
            )}
          </div>
        </Card>
      ) : null}

      <ConfirmDialog
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
        title={t("common.confirmDelete")}
        description={t("recordings.delete.desc")}
        confirmLabel={t("common.delete")}
        variant="danger"
        onConfirm={async () => {
          await del.mutateAsync({ recordingId: id });
          toast.ok(t("recordings.deleted"));
          onDeleted();
        }}
      />
    </>
  );
}
