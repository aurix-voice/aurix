import { Download, FileJson, Gavel, ShieldCheck, X } from "lucide-react";
import { useState } from "react";

import { downloadJson, downloadRaw, errorMessage, type T } from "@/api/client";
import { useApi, useModerationEventQuery, useResolveModerationEventMutation, useSafetyIncidentQuery, useUserRiskQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtDateTime, fmtDuration, fmtRelative, shortId } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { FormDialog } from "@/ui/Dialog";
import { Textarea } from "@/ui/Input";
import { QueryError } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, CodeBlock, Field, IdChip, KV, Mono, Skeleton } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { BanDialog } from "./BanDialog";
import { asNumber, asRecord, asString, asStringArray, ChannelRef, EventTypeBadge, isSafetyType, RiskBadge, StatusBadge, UserRef } from "./shared";

/** Side panel for one moderation event; safety incidents get the evidence bundle and user risk. */
export function EventDetail({ eventId, onClose }: { eventId: string; onClose: () => void }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const now = useNow(30_000);
  const event = useModerationEventQuery(eventId);
  const ev = event.data;
  const safety = ev ? isSafetyType(ev.event_type) : false;
  const incident = useSafetyIncidentQuery(safety ? eventId : null);
  const risk = useUserRiskQuery(ev?.target_user_id ?? null);
  const [resolving, setResolving] = useState(false);
  const [banning, setBanning] = useState(false);
  const [showRaw, setShowRaw] = useState(false);

  return (
    <Card className="flex flex-col gap-4 p-4">
      <CardHeader
        className="p-0"
        title={
          <span className="flex items-center gap-2">
            {t("moderation.event")}
            {ev ? <EventTypeBadge type={ev.event_type} /> : null}
            {ev ? <StatusBadge status={ev.status} /> : null}
          </span>
        }
        description={<IdChip id={eventId} />}
        actions={
          <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
            <X className="size-4" />
          </Button>
        }
      />

      {event.isError ? <QueryError error={event.error} onRetry={() => void event.refetch()} compact /> : null}
      {event.isPending ? <Skeleton className="h-40" /> : null}

      {ev ? (
        <>
          <KV
            cols={2}
            items={[
              { k: t("moderation.target"), v: <UserRef id={ev.target_user_id} /> },
              { k: t("common.channel"), v: <ChannelRef id={ev.channel_id} /> },
              { k: t("moderation.reporter"), v: <UserRef id={ev.reporter_user_id} /> },
              { k: t("moderation.moderator"), v: <UserRef id={ev.moderator_user_id} /> },
              { k: t("common.created"), v: <span title={fmtDateTime(locale, ev.created_at)}>{fmtRelative(locale, ev.created_at, now)}</span> },
              { k: t("moderation.resolved"), v: ev.resolved_at ? <span title={fmtDateTime(locale, ev.resolved_at)}>{fmtRelative(locale, ev.resolved_at, now)}</span> : <span className="text-fg-faint">—</span> },
              { k: t("live.recording"), v: ev.recording_id ? <IdChip id={ev.recording_id} /> : <span className="text-fg-faint">—</span>, hidden: !ev.recording_id },
            ]}
          />

          <div>
            <div className="text-[11px] font-medium uppercase tracking-wide text-fg-faint">{t("common.reason")}</div>
            <p className="mt-1 whitespace-pre-wrap break-words text-[13px]">{ev.reason || <span className="text-fg-faint">—</span>}</p>
          </div>

          {ev.resolution ? (
            <Callout tone="ok" title={t("moderation.resolution")}>
              <span className="whitespace-pre-wrap break-words">{ev.resolution}</span>
            </Callout>
          ) : null}

          {risk.data ? (
            <div className="flex items-center justify-between gap-3 rounded-xl border border-border bg-surface-2 px-3 py-2">
              <div className="text-[12.5px] text-fg-muted">{t("moderation.incident.risk")}</div>
              <div className="flex items-center gap-2">
                <RiskBadge risk={risk.data} />
                <span className="text-[12px] text-fg-muted">
                  {t("moderation.risk.incidents")}: {risk.data.incidents}
                </span>
              </div>
            </div>
          ) : null}

          {safety ? <IncidentEvidence incident={incident.data} loading={incident.isPending} error={incident.isError ? incident.error : null} retry={() => void incident.refetch()} /> : null}

          {ev.evidence != null ? (
            <div className="flex flex-col gap-1.5">
              <button type="button" className="self-start text-[12px] text-fg-muted hover:text-fg" onClick={() => setShowRaw((v) => !v)}>
                {showRaw ? t("common.hide") : t("common.show")} {t("common.raw").toLowerCase()} {t("moderation.evidence").toLowerCase()}
              </button>
              {showRaw ? <CodeBlock value={JSON.stringify(ev.evidence, null, 2)} maxHeight="20rem" /> : null}
            </div>
          ) : null}

          <div className="flex flex-wrap items-center gap-2 border-t border-border pt-3">
            {canApp("moderation:write") && ev.status !== "resolved" ? (
              <Button size="sm" onClick={() => setResolving(true)}>
                <ShieldCheck className="size-3.5" /> {t("moderation.resolve")}
              </Button>
            ) : null}
            {canApp("moderation:write") ? (
              <Button size="sm" variant="outline" onClick={() => setBanning(true)}>
                <Gavel className="size-3.5" /> {t("moderation.ban")}
              </Button>
            ) : null}
            {safety && incident.data ? (
              <Button size="sm" variant="ghost" onClick={() => downloadJson(incident.data, `incident-${shortId(eventId)}.json`)}>
                <FileJson className="size-3.5" /> {t("moderation.incident.export")}
              </Button>
            ) : null}
          </div>

          <ResolveDialog open={resolving} onClose={() => setResolving(false)} eventId={eventId} />
          <BanDialog open={banning} onClose={() => setBanning(false)} userId={ev.target_user_id} who={shortId(ev.target_user_id)} channelId={ev.channel_id ?? undefined} />
        </>
      ) : null}
    </Card>
  );
}

function ResolveDialog({ open, onClose, eventId }: { open: boolean; onClose: () => void; eventId: string }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const resolve = useResolveModerationEventMutation();
  const [text, setText] = useState("");
  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        if (!o) {
          setText("");
          onClose();
        }
      }}
      title={t("moderation.resolve")}
      description={t("moderation.resolve.desc")}
      submitLabel={t("moderation.resolve")}
      disabled={!text.trim()}
      onSubmit={async () => {
        try {
          await resolve.mutateAsync({ eventId, body: { resolution: text.trim() } });
          toast.ok(t("moderation.resolveDone"));
        } catch (e) {
          throw new Error(errorMessage(e, locale), { cause: e });
        }
      }}
    >
      <Field label={t("moderation.resolution")} required>
        <Textarea value={text} onChange={(e) => setText(e.target.value)} rows={3} maxLength={2000} autoFocus />
      </Field>
    </FormDialog>
  );
}

function IncidentEvidence({ incident, loading, error, retry }: { incident: T.SafetyIncident | undefined; loading: boolean; error: unknown; retry: () => void }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const api = useApi();
  const toast = useToast();
  const [downloading, setDownloading] = useState(false);

  if (error) return <QueryError error={error} onRetry={retry} compact />;
  if (loading || !incident) return <Skeleton className="h-24" />;

  const evidence = asRecord(incident.incident.evidence);
  const score = asNumber(evidence?.score);
  const categories = asStringArray(evidence?.categories);
  const masked = asString(evidence?.masked_text);
  const language = asString(evidence?.language);
  const blocked = evidence?.blocked === true;
  const sessionId = asString(evidence?.session_id);
  const classifier = asRecord(evidence?.classifier);
  const lexicon = asRecord(evidence?.lexicon);
  const lexiconMatches = Array.isArray(lexicon?.matches) ? lexicon.matches.map(asRecord).filter((m): m is Record<string, unknown> => m !== null) : [];
  const riskInfo = asRecord(evidence?.risk);
  const actions = asStringArray(evidence?.actions);
  const audio = incident.audio ?? null;
  const text = incident.text ?? asString(evidence?.text);

  const downloadAudio = async () => {
    if (!audio) return;
    setDownloading(true);
    try {
      const res = await api.downloadRecordingRaw(audio.recording_id);
      const ext = audio.format.includes("wav") ? "wav" : "ogg";
      downloadRaw(res, `evidence-${shortId(incident.incident.id)}.${ext}`);
    } catch (e) {
      toast.error(errorMessage(e, locale));
    } finally {
      setDownloading(false);
    }
  };

  return (
    <div className="flex flex-col gap-3 rounded-xl border border-border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[11px] font-medium uppercase tracking-wide text-fg-faint">{t("moderation.evidence")}</span>
        {incident.source ? <Badge tone="accent">{incident.source === "voice" ? t("moderation.source.voice") : t("moderation.source.text")}</Badge> : null}
        {score !== null ? <Badge tone={score >= 0.8 ? "danger" : score >= 0.5 ? "warn" : "neutral"}>{t("moderation.risk.score")}: {score.toFixed(2)}</Badge> : null}
        {blocked ? <Badge tone="danger">{t("moderation.incident.blocked")}</Badge> : null}
        {language ? <Badge>{language}</Badge> : null}
        {actions.map((a) => (
          <Badge key={a} tone="warn">
            {t("moderation.incident.action")}: {a}
          </Badge>
        ))}
      </div>

      {categories.length ? (
        <div className="flex flex-wrap gap-1">
          {categories.map((c) => (
            <Badge key={c}>{c}</Badge>
          ))}
        </div>
      ) : null}

      {text ? (
        <div>
          <div className="text-[11px] text-fg-faint">{t("moderation.incident.text")}</div>
          <p className="mt-0.5 whitespace-pre-wrap break-words rounded-lg bg-surface-2 px-2.5 py-1.5 text-[13px]">{text}</p>
          {masked && masked !== text ? <p className="mt-1 text-[12px] text-fg-muted">{t("moderation.incident.masked")}: {masked}</p> : null}
        </div>
      ) : null}

      {incident.context.length ? (
        <div>
          <div className="text-[11px] text-fg-faint">{t("moderation.incident.context")}</div>
          <ul className="mt-1 flex flex-col gap-1">
            {incident.context.map((c, i) => (
              <li key={c.message_id ?? i} className="flex gap-2 text-[12.5px]">
                <span className="shrink-0 text-fg-faint" title={fmtDateTime(locale, c.sent_at)}>
                  {c.sent_at ? new Date(c.sent_at).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : ""}
                </span>
                <span className="shrink-0 font-medium">{c.display_name || (c.from_user_id ? shortId(c.from_user_id) : "?")}</span>
                <span className="min-w-0 break-words text-fg-muted">{c.text}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {(classifier || lexiconMatches.length) ? (
        <KV
          cols={2}
          items={[
            { k: t("moderation.incident.classifier"), v: classifier ? <Mono>{asString(classifier.provider) ?? "?"}{asNumber(classifier.score) !== null ? ` · ${asNumber(classifier.score)?.toFixed(2)}` : ""}</Mono> : null, hidden: !classifier },
            {
              k: t("moderation.incident.lexicon"),
              v: lexiconMatches.length ? (
                <span className="flex flex-wrap gap-1">
                  {lexiconMatches.map((m, i) => (
                    <Badge key={i} tone="warn">
                      {asString(m.pattern) ?? "?"}
                      {asString(m.category) ? ` · ${asString(m.category)}` : ""}
                    </Badge>
                  ))}
                </span>
              ) : null,
              hidden: lexiconMatches.length === 0,
            },
            { k: t("common.session"), v: sessionId ? <IdChip id={sessionId} /> : null, hidden: !sessionId },
            {
              k: t("moderation.incident.riskDelta"),
              v: riskInfo ? (
                <Mono>
                  {asNumber(riskInfo.before)?.toFixed(2) ?? "?"} → {asNumber(riskInfo.after)?.toFixed(2) ?? "?"}
                </Mono>
              ) : null,
              hidden: !riskInfo,
            },
          ]}
        />
      ) : null}

      {audio ? (
        <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-border bg-surface-2 px-2.5 py-2">
          <div className="text-[12.5px]">
            <div className="font-medium">{t("moderation.incident.audio")}</div>
            <div className="text-fg-muted">
              {fmtDuration(locale, audio.duration_secs)} · {fmtBytes(locale, audio.size_bytes)} · {audio.format}
              {audio.encrypted ? ` · ${t("moderation.incident.encrypted")}` : ""}
              {audio.expired ? ` · ${t("moderation.incident.expired")}` : ` · ${t("common.expires")} ${fmtRelative(locale, audio.expires_at)}`}
            </div>
          </div>
          {canApp("recordings:read") && !audio.expired ? (
            <Button size="sm" variant="outline" loading={downloading} onClick={() => void downloadAudio()}>
              <Download className="size-3.5" /> {t("common.download")}
            </Button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
