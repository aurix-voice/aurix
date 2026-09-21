import { Link, useNavigate } from "@tanstack/react-router";
import { ArrowLeft, MoreHorizontal } from "lucide-react";
import { useMemo, useState } from "react";

import type { T } from "@/api/client";
import { errorKind, errorMessage } from "@/api/client";
import { useAdminAppUsageQuery, useAppQuery, useDeleteAppMutation, useRotateAppKeyMutation, useUpdateAppMutation } from "@/api/hooks";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { chart, ChartCard, MosBadge, QuotaRow, TimeSeriesChart, type Point, type SeriesDef } from "@/components/charts";
import { RangePicker, stepFor, useRange } from "@/components/RangePicker";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtCompact, fmtDate, fmtDateTime, fmtDuration, fmtMinutes, fmtMos, fmtNumber, fmtPercent } from "@/lib/format";
import { appDetailRoute } from "@/router";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { Menu, Tabs } from "@/ui/Menu";
import { PageHeader, QueryError, Section } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, EmptyState, IdChip, KV, Skeleton, Stat } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { AppFormFields, parseLimit, type AppFormValues } from "./AppForm";
import { AppKeysTab } from "./AppKeysTab";
import { AppWebhooksTab } from "./AppWebhooksTab";
import { SecretReveal } from "./SecretReveal";

type Tab = "overview" | "keys" | "webhooks" | "usage" | "settings";
const TABS: readonly Tab[] = ["overview", "keys", "webhooks", "usage", "settings"];

function isTab(v: string | undefined): v is Tab {
  return TABS.some((t) => t === v);
}

export default function AppDetailPage() {
  const { appId } = appDetailRoute.useParams();
  const { tab } = appDetailRoute.useSearch();
  const navigate = useNavigate();
  const { t } = useI18n();
  const { can, canApp } = useAuth();
  const { appId: scopeId, setAppId } = useAppScope();
  const toast = useToast();

  const app = useAppQuery(appId);
  const rotate = useRotateAppKeyMutation(appId);
  const del = useDeleteAppMutation();
  const [rotating, setRotating] = useState(false);
  const [rotated, setRotated] = useState<T.RotateAppKeyResponse | null>(null);
  const [deleting, setDeleting] = useState(false);

  const current: Tab = isTab(tab) ? tab : "overview";
  const setTab = (v: string) => void navigate({ to: "/apps/$appId", params: { appId }, search: v === "overview" ? {} : { tab: v }, replace: true });

  if (!can("apps:read")) {
    return <EmptyState title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "apps:read" })} />;
  }

  if (app.isError) {
    const notFound = errorKind(app.error) === "notFound";
    return (
      <div className="flex flex-col gap-4">
        <BackLink />
        {notFound ? <EmptyState title={t("common.notFound")} description={t("apps.notFound")} /> : <QueryError error={app.error} onRetry={() => void app.refetch()} />}
      </div>
    );
  }

  const a = app.data;
  const inScope = scopeId === appId;

  return (
    <div className="flex flex-col gap-4">
      <BackLink />
      <PageHeader
        title={
          a ? (
            <span className="inline-flex items-center gap-2.5">
              {a.name}
              <Badge tone={a.active ? "ok" : "neutral"} dot>
                {a.active ? t("common.active") : t("common.inactive")}
              </Badge>
              {inScope ? <Badge tone="accent">{t("apps.inScope")}</Badge> : null}
            </span>
          ) : (
            <Skeleton className="h-7 w-48" />
          )
        }
        description={
          a ? (
            <span className="inline-flex items-center gap-3">
              <IdChip id={a.id} short={36} />
              {a.description ? <span>{a.description}</span> : null}
            </span>
          ) : undefined
        }
        actions={
          a ? (
            <>
              {!inScope && a.active ? (
                <Button size="sm" variant="outline" onClick={() => setAppId(a.id)}>
                  {t("apps.useAsScope")}
                </Button>
              ) : null}
              <Menu
                label={t("common.actions")}
                trigger={
                  <Button size="sm" variant="outline" aria-label={t("common.actions")}>
                    <MoreHorizontal className="size-4" />
                  </Button>
                }
                items={[
                  { label: t("apps.rotateKey"), onSelect: () => setRotating(true), hidden: !can("keys:rotate") || !a.active },
                  { label: t("apps.deactivate"), onSelect: () => setDeleting(true), danger: true, separatorBefore: true, hidden: !can("apps:delete") || !a.active },
                ]}
              />
            </>
          ) : null
        }
        tabs={
          <Tabs
            value={current}
            onValueChange={setTab}
            tabs={[
              { value: "overview", label: t("apps.tabs.overview") },
              { value: "keys", label: t("apps.tabs.keys"), hidden: !canApp("keys:manage") },
              { value: "webhooks", label: t("apps.tabs.webhooks"), hidden: !canApp("webhooks:read") },
              { value: "usage", label: t("apps.tabs.usage"), hidden: !can("analytics:read") },
              { value: "settings", label: t("apps.tabs.settings"), hidden: !can("apps:write") },
            ]}
          />
        }
        className="mb-0"
      />

      {a && !a.active ? <Callout tone="warn">{t("apps.inactive.notice")}</Callout> : null}

      {current === "overview" ? <OverviewTab appId={appId} app={a} /> : null}
      {current === "keys" ? (
        <Card>
          <AppKeysTab appId={appId} />
        </Card>
      ) : null}
      {current === "webhooks" ? (
        <Card>
          <AppWebhooksTab appId={appId} />
        </Card>
      ) : null}
      {current === "usage" ? <UsageTab appId={appId} /> : null}
      {current === "settings" && a ? <SettingsTab app={a} /> : null}

      <ConfirmDialog open={rotating} onOpenChange={setRotating} title={t("apps.rotateKey")} description={t("apps.rotateKey.desc")} confirmLabel={t("apps.rotateKey")} onConfirm={async () => setRotated(await rotate.mutateAsync())} />
      <SecretReveal
        secret={rotated?.api_key ?? null}
        title={t("apps.rotated")}
        notice={t("apps.created.keyNotice")}
        meta={rotated ? [{ k: t("keys.default"), v: <IdChip id={rotated.api_key_id} short={36} /> }] : undefined}
        onClose={() => setRotated(null)}
      />
      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={t("apps.deactivate")}
        description={a ? t("apps.delete.desc", { name: a.name }) : undefined}
        confirmLabel={t("apps.deactivate")}
        variant="danger"
        onConfirm={async () => {
          await del.mutateAsync({ appId });
          if (inScope) setAppId(null);
          toast.ok(t("apps.deactivated"));
          void navigate({ to: "/apps" });
        }}
      />
    </div>
  );
}

function BackLink() {
  const { t } = useI18n();
  return (
    <Link to="/apps" className="inline-flex items-center gap-1 text-xs text-fg-muted hover:text-fg w-fit">
      <ArrowLeft className="size-3.5" />
      {t("apps.title")}
    </Link>
  );
}

function limitText(locale: "en" | "ru", v: number | undefined, unlimited: string): string {
  if (v === undefined) return "—";
  return v === 0 ? unlimited : fmtNumber(locale, v);
}

function OverviewTab({ appId, app }: { appId: string; app: T.AppDetail | undefined }) {
  const { t, locale } = useI18n();
  const { can } = useAuth();
  const [range] = useRange("24h");
  const usage = useAdminAppUsageQuery(appId, { from: range.from, to: range.to, step: stepFor(range) });
  const cur = usage.data?.current;
  const quota = usage.data?.quota;
  const skel = <Skeleton className="h-7 w-14" />;
  const showUsage = can("analytics:read");

  return (
    <div className="flex flex-col gap-4">
      {showUsage ? (
        <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
          <Stat label={t("apps.usage.sessions")} value={usage.isPending ? skel : fmtNumber(locale, cur?.active_sessions)} hint={t("apps.usage.current")} />
          <Stat label={t("apps.usage.channels")} value={usage.isPending ? skel : fmtNumber(locale, cur?.active_channels)} hint={t("apps.usage.current")} />
          <Stat label={t("apps.usage.users")} value={usage.isPending ? skel : fmtNumber(locale, cur?.users)} />
          <Stat label={t("apps.usage.monthMinutes")} value={usage.isPending ? skel : fmtMinutes(locale, quota?.participant_minutes_this_month)} hint={quota ? t("apps.quota.resets", { date: fmtDate(locale, nextMonth(quota.month_start)) }) : undefined} />
        </div>
      ) : null}
      <div className="grid lg:grid-cols-2 gap-4">
        <Card>
          <CardHeader title={t("common.details")} />
          <div className="px-4 pb-4">
            {app ? (
              <KV
                cols={2}
                items={[
                  { k: t("common.id"), v: <IdChip id={app.id} short={36} /> },
                  { k: t("apps.owner"), v: <IdChip id={app.owner_id} short={36} /> },
                  { k: t("common.created"), v: fmtDateTime(locale, app.created_at) },
                  { k: t("common.status"), v: app.active ? t("common.active") : t("common.inactive") },
                  { k: t("apps.maxChannels"), v: limitText(locale, app.max_channels, t("common.unlimited")) },
                  { k: t("apps.maxParticipantsPerChannel"), v: limitText(locale, app.max_participants_per_channel, t("common.unlimited")) },
                  { k: t("apps.maxConcurrentSessions"), v: limitText(locale, app.max_concurrent_sessions, t("common.unlimited")) },
                  { k: t("apps.monthlyParticipantMinutes"), v: limitText(locale, app.monthly_participant_minutes, t("common.unlimited")) },
                ]}
              />
            ) : (
              <div className="flex flex-col gap-2">
                <Skeleton className="h-8" />
                <Skeleton className="h-8" />
                <Skeleton className="h-8" />
              </div>
            )}
          </div>
        </Card>
        {showUsage ? (
          <Card>
            <CardHeader title={t("apps.quota.title")} description={quota ? t("apps.quota.resets", { date: fmtDate(locale, nextMonth(quota.month_start)) }) : undefined} />
            <div className="px-4 pb-4 flex flex-col gap-3">
              {usage.isPending ? (
                <>
                  <Skeleton className="h-8" />
                  <Skeleton className="h-8" />
                </>
              ) : usage.isError ? (
                <QueryError error={usage.error} onRetry={() => void usage.refetch()} compact />
              ) : quota ? (
                <>
                  <QuotaRow label={t("overview.quota.ccu")} used={quota.active_sessions} limit={quota.max_concurrent_sessions} />
                  <QuotaRow label={t("overview.quota.minutes")} used={quota.participant_minutes_this_month} limit={quota.monthly_participant_minutes} minutes />
                </>
              ) : null}
            </div>
          </Card>
        ) : null}
      </div>
    </div>
  );
}

function nextMonth(monthStart: string): string {
  const d = new Date(monthStart);
  return new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1)).toISOString();
}

function UsageTab({ appId }: { appId: string }) {
  const { t, locale } = useI18n();
  const [range, setRange] = useRange("7d");
  const usage = useAdminAppUsageQuery(appId, { from: range.from, to: range.to, step: stepFor(range) });
  const totals = usage.data?.totals;

  const loadPoints = useMemo<Point[]>(
    () => (usage.data?.series ?? []).map((b) => ({ t: Date.parse(b.bucket), sessions: b.peak_sessions, participants: b.peak_participants, minutes: b.participant_minutes })),
    [usage.data],
  );
  const qualityPoints = useMemo<Point[]>(
    () =>
      (usage.data?.series ?? []).map((b) => ({
        t: Date.parse(b.bucket),
        mos: b.quality_samples > 0 ? b.mos_sum_milli / b.quality_samples / 1000 : null,
        loss: b.quality_samples > 0 ? b.loss_sum_permille / b.quality_samples / 10 : null,
        poor: b.quality_samples > 0 ? (b.poor_quality_samples / b.quality_samples) * 100 : null,
      })),
    [usage.data],
  );
  const loadSeries: SeriesDef[] = [
    { key: "sessions", label: t("apps.usage.peakSessions"), color: chart.fg, area: true },
    { key: "participants", label: t("apps.usage.peakParticipants"), color: chart.accent },
    { key: "minutes", label: t("apps.usage.participantMinutes"), color: chart.muted, right: true },
  ];
  const qualitySeries: SeriesDef[] = [
    { key: "mos", label: t("overview.mos"), color: chart.ok, format: fmtMos },
    { key: "loss", label: t("overview.loss"), color: chart.warn, right: true, format: (v) => fmtPercent(locale, v) },
    { key: "poor", label: t("overview.poor"), color: chart.danger, right: true, format: (v) => fmtPercent(locale, v) },
  ];

  const mos = totals && totals.quality_samples > 0 ? totals.mos_sum_milli / totals.quality_samples / 1000 : null;
  const poorPct = totals && totals.quality_samples > 0 ? (totals.poor_quality_samples / totals.quality_samples) * 100 : null;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[13px] font-semibold text-fg-muted">{t("apps.usage.range")}</span>
        <RangePicker value={range} onChange={setRange} />
      </div>
      <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
        <Stat label={t("apps.usage.peakSessions")} value={usage.isPending ? <Skeleton className="h-7 w-14" /> : fmtNumber(locale, totals?.peak_sessions)} />
        <Stat label={t("apps.usage.participantMinutes")} value={usage.isPending ? <Skeleton className="h-7 w-14" /> : fmtMinutes(locale, totals?.participant_minutes)} />
        <Stat label={t("overview.mos")} value={usage.isPending ? <Skeleton className="h-7 w-14" /> : <MosBadge mos={mos} />} hint={totals ? t("common.count.items", { n: totals.quality_samples }) : undefined} />
        <Stat label={t("overview.poor")} value={usage.isPending ? <Skeleton className="h-7 w-14" /> : fmtPercent(locale, poorPct)} />
      </div>
      <div className="grid lg:grid-cols-2 gap-4">
        <ChartCard title={t("apps.usage.chart")} description={t("overview.usageChart.desc")} series={loadSeries} query={usage}>
          <TimeSeriesChart data={loadPoints} series={loadSeries} empty={t("common.empty")} />
        </ChartCard>
        <ChartCard title={t("apps.usage.qualityChart")} description={t("overview.qualityChart.desc")} series={qualitySeries} query={usage}>
          <TimeSeriesChart data={qualityPoints} series={qualitySeries} leftDomain={[1, 4.5]} rightDomain={[0, "auto"]} empty={t("common.empty")} />
        </ChartCard>
      </div>
      <Card>
        <CardHeader title={t("apps.usage.range")} description={usage.data ? `${fmtDateTime(locale, usage.data.range.from)} → ${fmtDateTime(locale, usage.data.range.to)}` : undefined} />
        <div className="px-4 pb-4">
          {usage.isError ? (
            <QueryError error={usage.error} onRetry={() => void usage.refetch()} compact />
          ) : totals ? (
            <KV
              cols={3}
              items={[
                { k: t("apps.usage.sessionsStarted"), v: fmtNumber(locale, totals.sessions_started) },
                { k: t("apps.usage.sessionMinutes"), v: fmtMinutes(locale, totals.session_minutes) },
                { k: t("apps.usage.peakParticipants"), v: fmtNumber(locale, totals.peak_participants) },
                { k: t("apps.usage.recording"), v: fmtDuration(locale, totals.recording_seconds) },
                { k: t("apps.usage.media"), v: `${fmtBytes(locale, totals.media_bytes_in)} / ${fmtBytes(locale, totals.media_bytes_out)}` },
                { k: t("apps.usage.chat"), v: fmtCompact(locale, totals.chat_messages) },
                { k: t("apps.usage.tts"), v: `${fmtCompact(locale, totals.tts_requests)} / ${fmtCompact(locale, totals.tts_characters)}` },
                { k: t("apps.usage.stt"), v: fmtDuration(locale, Math.round(totals.stt_audio_ms / 1000)) },
              ]}
            />
          ) : (
            <Skeleton className="h-24" />
          )}
        </div>
      </Card>
    </div>
  );
}

function toForm(app: T.AppDetail): AppFormValues {
  const s = (v: number | undefined) => (v === undefined ? "" : String(v));
  return {
    name: app.name,
    description: app.description ?? "",
    max_channels: s(app.max_channels),
    max_participants_per_channel: s(app.max_participants_per_channel),
    max_concurrent_sessions: s(app.max_concurrent_sessions),
    monthly_participant_minutes: s(app.monthly_participant_minutes),
  };
}

function SettingsTab({ app }: { app: T.AppDetail }) {
  const { t, locale } = useI18n();
  const { can } = useAuth();
  const toast = useToast();
  const update = useUpdateAppMutation(app.id);
  const [edited, setEdited] = useState<{ base: T.AppDetail; form: AppFormValues } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const form = edited && edited.base === app ? edited.form : toForm(app);
  const setForm = (f: AppFormValues) => setEdited({ base: app, form: f });

  const dirty = JSON.stringify(form) !== JSON.stringify(toForm(app));

  const save = async () => {
    setError(null);
    const name = form.name.trim();
    if (!name) return setError(t("common.required"));
    const limits = {
      max_channels: parseLimit(form.max_channels, 1),
      max_participants_per_channel: parseLimit(form.max_participants_per_channel, 1),
      max_concurrent_sessions: parseLimit(form.max_concurrent_sessions),
      monthly_participant_minutes: parseLimit(form.monthly_participant_minutes),
    };
    if (limits.max_channels === null || limits.max_participants_per_channel === null || limits.max_concurrent_sessions === null || limits.monthly_participant_minutes === null) {
      return setError(t("apps.limits.invalid"));
    }
    const body: T.UpdateAppRequest = {};
    if (name !== app.name) body.name = name;
    const desc = form.description.trim();
    if (desc !== (app.description ?? "")) body.description = desc || null;
    if (limits.max_channels !== undefined && limits.max_channels !== app.max_channels) body.max_channels = limits.max_channels;
    if (limits.max_participants_per_channel !== undefined && limits.max_participants_per_channel !== app.max_participants_per_channel) body.max_participants_per_channel = limits.max_participants_per_channel;
    if (limits.max_concurrent_sessions !== undefined && limits.max_concurrent_sessions !== app.max_concurrent_sessions) body.max_concurrent_sessions = limits.max_concurrent_sessions;
    if (limits.monthly_participant_minutes !== undefined && limits.monthly_participant_minutes !== app.monthly_participant_minutes) body.monthly_participant_minutes = limits.monthly_participant_minutes;
    if (Object.keys(body).length === 0) return;
    try {
      await update.mutateAsync(body);
      setEdited(null);
      toast.ok(t("apps.saved"));
    } catch (e) {
      setError(errorMessage(e, locale));
    }
  };

  return (
    <div className="grid lg:grid-cols-[minmax(0,40rem)] gap-4">
      <Card>
        <CardHeader title={t("apps.tabs.settings")} description={t("apps.settings.desc")} />
        <form
          className="px-4 pb-4 flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <AppFormFields value={form} onChange={setForm} />
          {error ? <Callout tone="danger">{error}</Callout> : null}
          <div className="flex items-center justify-end gap-2">
            <Button type="button" variant="ghost" size="sm" disabled={!dirty || update.isPending} onClick={() => setEdited(null)}>
              {t("common.reset")}
            </Button>
            <Button type="submit" variant="primary" size="sm" disabled={!dirty || !can("apps:write") || !app.active} loading={update.isPending}>
              {t("common.save")}
            </Button>
          </div>
        </form>
      </Card>
      {can("apps:delete") && app.active ? (
        <Section title={t("apps.danger")}>
          <Card className="border-danger/40">
            <div className="px-4 py-3.5 flex items-center justify-between gap-4">
              <p className="text-[13px] text-fg-muted max-w-lg">{t("apps.delete.desc", { name: app.name })}</p>
              <DeactivateButton app={app} />
            </div>
          </Card>
        </Section>
      ) : null}
    </div>
  );
}

function DeactivateButton({ app }: { app: T.AppDetail }) {
  const { t } = useI18n();
  const navigate = useNavigate();
  const toast = useToast();
  const { appId: scopeId, setAppId } = useAppScope();
  const del = useDeleteAppMutation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button variant="danger" size="sm" onClick={() => setOpen(true)}>
        {t("apps.deactivate.button")}
      </Button>
      <ConfirmDialog
        open={open}
        onOpenChange={setOpen}
        title={t("apps.deactivate")}
        description={t("apps.delete.desc", { name: app.name })}
        confirmLabel={t("apps.deactivate")}
        variant="danger"
        onConfirm={async () => {
          await del.mutateAsync({ appId: app.id });
          if (scopeId === app.id) setAppId(null);
          toast.ok(t("apps.deactivated"));
          void navigate({ to: "/apps" });
        }}
      />
    </>
  );
}
