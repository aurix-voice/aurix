import { useNavigate } from "@tanstack/react-router";
import { BarChart3, Download, RefreshCw, X } from "lucide-react";
import { useMemo, useState } from "react";

import { downloadJson, downloadRaw, errorMessage, type T } from "@/api/client";
import { useApi, useAppAnalyticsQuery, useChannelsQuery, useChannelUsageListQuery, useChannelUsageQuery, useQuotaQuery, useSessionQualityQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { chart, ChartCard, MosBadge, mosTone, QuotaRow, TimeSeriesChart, type SeriesDef } from "@/components/charts";
import { RangePicker, stepFor, useRange, type Range } from "@/components/RangePicker";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtCompact, fmtDateTime, fmtDuration, fmtMinutes, fmtMos, fmtNumber, fmtPercent, fmtRelative, shortId } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { analyticsRoute } from "@/router";
import { Forbidden, RequireApp } from "@/shell/Guards";
import { Button } from "@/ui/Button";
import { NativeSelect } from "@/ui/Input";
import { Menu, Tabs } from "@/ui/Menu";
import { PageHeader, QueryError, SplitLayout, Toolbar } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, CopyButton, EmptyState, KV, Mono, Skeleton, Stat } from "@/ui/Primitives";
import { DataTable, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { ChannelRef, UserRef } from "../moderation/shared";
import {
  ANALYTICS_TABS,
  barsPercent,
  channelPoints,
  chatPoints,
  exportFilename,
  latencyPoints,
  parseTab,
  qualityPoints,
  rankChannels,
  rankSessions,
  trafficPoints,
  usagePoints,
  type AnalyticsTab,
  type ChannelSortKey,
} from "./model";

const SK = () => <Skeleton className="h-7 w-16" />;
const ms = (locale: Parameters<typeof fmtNumber>[0], v: number | null | undefined, digits = 0) => (v === null || v === undefined ? "—" : `${fmtNumber(locale, v, digits)} ms`);

function useAnalyticsSearch(): { tab: AnalyticsTab; go: (tab: AnalyticsTab) => void } {
  const search = analyticsRoute.useSearch();
  const navigate = useNavigate();
  return {
    tab: parseTab(search.tab),
    go: (tab) => void navigate({ to: "/analytics", search: { tab }, replace: true }),
  };
}

function ExportMenu({ range }: { range: Range }) {
  const { t, locale } = useI18n();
  const api = useApi();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const run = async (scope: "app" | "channels", format: "json" | "csv") => {
    setBusy(true);
    try {
      const q = { from: range.from, to: range.to, scope, format };
      if (format === "csv") {
        downloadRaw(await api.exportUsageRaw(q), exportFilename(scope, format, range));
      } else {
        const res = await api.exportUsage(q);
        downloadJson(res, exportFilename(scope, format, range));
        if (res.truncated) toast.push({ kind: "info", title: t("analytics.export.truncated", { n: res.count }) });
      }
    } catch (e) {
      toast.error(errorMessage(e, locale));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Menu
      label={t("common.export")}
      trigger={
        <Button variant="secondary" size="sm" loading={busy}>
          <Download className="size-3.5" />
          {t("common.export")}
        </Button>
      }
      items={[
        { label: `${t("analytics.exportCsv")} · ${t("analytics.scope.app")}`, onSelect: () => void run("app", "csv") },
        { label: `${t("analytics.exportCsv")} · ${t("analytics.scope.channels")}`, onSelect: () => void run("channels", "csv") },
        { label: `${t("analytics.exportJson")} · ${t("analytics.scope.app")}`, onSelect: () => void run("app", "json"), separatorBefore: true },
        { label: `${t("analytics.exportJson")} · ${t("analytics.scope.channels")}`, onSelect: () => void run("channels", "json") },
      ]}
    />
  );
}

function UsageTab({ range }: { range: Range }) {
  const { t, locale } = useI18n();
  const usage = useAppAnalyticsQuery({ from: range.from, to: range.to, step: stepFor(range) });
  const quota = useQuotaQuery();
  const series = usage.data?.series;
  const totals = usage.data?.totals;
  const cur = usage.data?.current;

  const load = useMemo(() => usagePoints(series ?? []), [series]);
  const traffic = useMemo(() => trafficPoints(series ?? []), [series]);
  const chat = useMemo(() => chatPoints(series ?? []), [series]);
  const loadSeries: SeriesDef[] = [
    { key: "sessions", label: t("analytics.peakSessions"), color: chart.fg, area: true },
    { key: "participants", label: t("analytics.peakParticipants"), color: chart.accent },
    { key: "started", label: t("analytics.sessionsStarted"), color: chart.faint },
    { key: "minutes", label: t("analytics.participantMinutes"), color: chart.muted, right: true, format: (v) => fmtMinutes(locale, v) },
  ];
  const trafficSeries: SeriesDef[] = [
    { key: "in", label: t("analytics.mediaIn"), color: chart.accent, area: true, format: (v) => fmtBytes(locale, v) },
    { key: "out", label: t("analytics.mediaOut"), color: chart.fg, format: (v) => fmtBytes(locale, v) },
    { key: "recording", label: t("analytics.recordingSeconds"), color: chart.warn, right: true, format: (v) => fmtDuration(locale, v) },
  ];
  const chatSeries: SeriesDef[] = [
    { key: "chat", label: t("analytics.chatMessages"), color: chart.fg, area: true },
    { key: "tts", label: t("analytics.tts"), color: chart.accent },
    { key: "stt", label: t("analytics.stt"), color: chart.warn, right: true, format: (v) => fmtMinutes(locale, v) },
  ];

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
        <Stat label={t("overview.ccu")} value={usage.isPending ? <SK /> : fmtNumber(locale, cur?.active_sessions)} hint={cur ? t("analytics.activeChannels", { n: cur.active_channels }) : undefined} />
        <Stat label={t("analytics.peakSessions")} value={usage.isPending ? <SK /> : fmtNumber(locale, totals?.peak_sessions)} hint={totals ? t("analytics.peakParticipantsHint", { n: totals.peak_participants }) : undefined} />
        <Stat label={t("analytics.participantMinutes")} value={usage.isPending ? <SK /> : fmtMinutes(locale, totals?.participant_minutes)} hint={totals ? t("analytics.sessionMinutesHint", { n: fmtMinutes(locale, totals.session_minutes) }) : undefined} />
        <Stat label={t("analytics.sessionsStarted")} value={usage.isPending ? <SK /> : fmtNumber(locale, totals?.sessions_started)} hint={cur ? t("analytics.usersKnown", { n: fmtNumber(locale, cur.users) }) : undefined} />
      </div>
      <ChartCard title={t("analytics.usageChart")} description={t("analytics.usageChart.desc")} series={loadSeries} query={usage}>
        <TimeSeriesChart data={load} series={loadSeries} empty={t("common.empty")} />
      </ChartCard>
      <div className="grid lg:grid-cols-2 gap-4">
        <ChartCard title={t("analytics.trafficChart")} description={t("analytics.trafficChart.desc")} series={trafficSeries} query={usage}>
          <TimeSeriesChart data={traffic} series={trafficSeries} empty={t("common.empty")} />
        </ChartCard>
        <ChartCard title={t("analytics.chatChart")} description={t("analytics.chatChart.desc")} series={chatSeries} query={usage}>
          <TimeSeriesChart data={chat} series={chatSeries} empty={t("common.empty")} />
        </ChartCard>
      </div>
      <div className="grid lg:grid-cols-[minmax(0,1fr)_320px] gap-4 items-start">
        <Card>
          <CardHeader
            title={t("common.total")}
            description={
              usage.data
                ? t("analytics.rangeDesc", { from: fmtDateTime(locale, usage.data.range.from), to: fmtDateTime(locale, usage.data.range.to), step: fmtDuration(locale, usage.data.range.step_secs) })
                : undefined
            }
          />
          <div className="px-4 pb-4">
            {usage.isError ? (
              <QueryError error={usage.error} onRetry={() => void usage.refetch()} compact />
            ) : totals && usage.data ? (
              <>
                <KV
                  cols={3}
                  items={[
                    { k: t("analytics.peakSessions"), v: fmtNumber(locale, totals.peak_sessions) },
                    { k: t("analytics.peakParticipants"), v: fmtNumber(locale, totals.peak_participants) },
                    { k: t("analytics.sessionsStarted"), v: fmtNumber(locale, totals.sessions_started) },
                    { k: t("analytics.sessionMinutes"), v: fmtMinutes(locale, totals.session_minutes) },
                    { k: t("analytics.participantMinutes"), v: fmtMinutes(locale, totals.participant_minutes) },
                    { k: t("analytics.recordingSeconds"), v: fmtDuration(locale, totals.recording_seconds) },
                    { k: t("analytics.mediaIn"), v: fmtBytes(locale, totals.media_bytes_in) },
                    { k: t("analytics.mediaOut"), v: fmtBytes(locale, totals.media_bytes_out) },
                    { k: t("analytics.chatMessages"), v: fmtNumber(locale, totals.chat_messages) },
                    { k: t("analytics.tts"), v: fmtNumber(locale, totals.tts_requests) },
                    { k: t("analytics.ttsChars"), v: fmtCompact(locale, totals.tts_characters) },
                    { k: t("analytics.stt"), v: fmtMinutes(locale, totals.stt_audio_ms / 60_000) },
                  ]}
                />
                <p className="mt-3 text-xs text-fg-faint">
                  {usage.data.range.finalized_through ? t("analytics.finalized", { when: fmtDateTime(locale, usage.data.range.finalized_through) }) : t("analytics.notFinalized")}
                </p>
              </>
            ) : (
              <Skeleton className="h-32" />
            )}
          </div>
        </Card>
        <Card>
          <CardHeader title={t("analytics.quota")} description={t("analytics.quota.desc")} />
          <div className="px-4 pb-4 flex flex-col gap-3">
            {quota.isError ? (
              <QueryError error={quota.error} onRetry={() => void quota.refetch()} compact />
            ) : quota.data ? (
              <>
                <QuotaRow label={t("overview.quota.ccu")} used={quota.data.active_sessions} limit={quota.data.max_concurrent_sessions} />
                <QuotaRow label={t("overview.quota.minutes")} used={quota.data.participant_minutes_this_month} limit={quota.data.monthly_participant_minutes} minutes />
                <p className="text-xs text-fg-faint">{t("analytics.quota.since", { when: fmtDateTime(locale, quota.data.month_start) })}</p>
              </>
            ) : (
              <Skeleton className="h-20" />
            )}
          </div>
        </Card>
      </div>
    </div>
  );
}

const BAR_TONES = ["bg-danger", "bg-danger", "bg-warn", "bg-ok", "bg-ok"];

function Bars({ bars }: { bars: readonly number[] }) {
  const pct = barsPercent(bars);
  return (
    <div className="flex h-2 w-24 overflow-hidden rounded-full bg-surface-2" title={pct.map((p, i) => `${i}: ${Math.round(p)}%`).join(" · ")}>
      {pct.map((p, i) => (p > 0 ? <span key={i} className={BAR_TONES[i] ?? "bg-fg"} style={{ width: `${p}%` }} /> : null))}
    </div>
  );
}

function SessionCard({ s, onClose }: { s: T.SessionQuality; onClose: () => void }) {
  const { t, locale } = useI18n();
  const q = s.quality;
  return (
    <Card>
      <CardHeader
        title={
          <span className="inline-flex items-center gap-2">
            <Mono>{shortId(s.session_id, 12)}</Mono>
            <CopyButton value={s.session_id} />
          </span>
        }
        actions={
          <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
            <X className="size-4" />
          </Button>
        }
      />
      <div className="px-4 pb-4 flex flex-col gap-3">
        <div className="flex items-center gap-2 flex-wrap">
          <MosBadge mos={q.mos_avg} />
          {q.mos_alerts > 0 ? <Badge tone="danger">{t("analytics.alertsCount", { n: q.mos_alerts })}</Badge> : null}
          {s.disconnected_at ? null : (
            <Badge tone="ok" dot>
              {t("analytics.stillConnected")}
            </Badge>
          )}
        </div>
        <KV
          cols={2}
          items={[
            { k: t("common.user"), v: <UserRef id={s.user_id} /> },
            { k: t("common.node"), v: s.media_node_id ? <Mono>{s.media_node_id}</Mono> : "—" },
            { k: t("analytics.connected"), v: fmtDateTime(locale, s.connected_at) },
            { k: t("analytics.disconnected"), v: s.disconnected_at ? fmtDateTime(locale, s.disconnected_at) : t("analytics.stillConnected") },
            { k: t("analytics.disconnectReason"), v: s.disconnect_reason ?? "—" },
            { k: t("analytics.ratedTime"), v: fmtDuration(locale, q.seconds) },
            { k: t("analytics.mosAvg"), v: fmtMos(q.mos_avg) },
            { k: t("analytics.mosMin"), v: fmtMos(q.mos_min) },
            { k: t("analytics.mosLast"), v: fmtMos(q.mos_last) },
            { k: t("analytics.rFactor"), v: fmtNumber(locale, q.r_factor_avg, 1) },
            { k: t("analytics.rtt"), v: `${ms(locale, q.rtt_avg_ms)} / ${ms(locale, q.rtt_max_ms)}` },
            { k: t("analytics.jitter"), v: ms(locale, q.jitter_avg_ms, 1) },
            { k: t("analytics.loss"), v: `${fmtPercent(locale, q.loss_avg_percent)} / ${fmtPercent(locale, q.loss_max_percent)}` },
            { k: t("analytics.poorSeconds"), v: fmtDuration(locale, q.poor_seconds) },
            { k: t("analytics.samples"), v: fmtNumber(locale, q.samples) },
            { k: t("analytics.bars"), v: <Bars bars={q.bars} /> },
          ]}
        />
      </div>
    </Card>
  );
}

const MIN_SAMPLES = [1, 3, 10, 30] as const;
const LIMITS = [25, 50, 100] as const;

function QualityTab({ range }: { range: Range }) {
  const { t, locale } = useI18n();
  const now = useNow(30_000);
  const usage = useAppAnalyticsQuery({ from: range.from, to: range.to, step: stepFor(range) });
  const [minSamples, setMinSamples] = useState<number>(3);
  const [limit, setLimit] = useState<number>(50);
  const [selected, setSelected] = useState<string | null>(null);
  const worst = useSessionQualityQuery({ from: range.from, to: range.to, limit, min_samples: minSamples });
  const series = usage.data?.series;
  const agg = usage.data?.totals.quality;

  const quality = useMemo(() => qualityPoints(series ?? []), [series]);
  const latency = useMemo(() => latencyPoints(series ?? []), [series]);
  const qualitySeries: SeriesDef[] = [
    { key: "mos", label: t("analytics.mos"), color: chart.ok, format: fmtMos },
    { key: "loss", label: t("analytics.loss"), color: chart.warn, right: true, format: (v) => fmtPercent(locale, v) },
    { key: "poor", label: t("analytics.poor"), color: chart.danger, right: true, format: (v) => fmtPercent(locale, v) },
  ];
  const latencySeries: SeriesDef[] = [
    { key: "rtt", label: t("analytics.rtt"), color: chart.accent, format: (v) => ms(locale, v) },
    { key: "jitter", label: t("analytics.jitter"), color: chart.fg, format: (v) => ms(locale, v, 1) },
    { key: "samples", label: t("analytics.samples"), color: chart.faint, right: true, area: true },
  ];

  const sessions = useMemo(() => rankSessions(worst.data?.sessions ?? []), [worst.data]);
  const sel = sessions.find((s) => s.session_id === selected) ?? null;
  const columns: Column<T.SessionQuality>[] = [
    { key: "mos", header: t("analytics.mosAvg"), sort: (s) => s.quality.mos_avg, cell: (s) => <MosBadge mos={s.quality.mos_avg} /> },
    {
      key: "min",
      header: t("analytics.mosMin"),
      align: "right",
      sort: (s) => s.quality.mos_min,
      cell: (s) => <span className={mosTone(s.quality.mos_min) === "danger" ? "text-danger tabular" : "tabular"}>{fmtMos(s.quality.mos_min)}</span>,
    },
    { key: "user", header: t("common.user"), cell: (s) => <UserRef id={s.user_id} /> },
    { key: "session", header: t("common.session"), cell: (s) => <Mono title={s.session_id}>{shortId(s.session_id)}</Mono> },
    { key: "rtt", header: t("analytics.rtt"), align: "right", sort: (s) => s.quality.rtt_avg_ms, cell: (s) => ms(locale, s.quality.rtt_avg_ms) },
    { key: "jitter", header: t("analytics.jitter"), align: "right", sort: (s) => s.quality.jitter_avg_ms, cell: (s) => ms(locale, s.quality.jitter_avg_ms, 1) },
    { key: "loss", header: t("analytics.loss"), align: "right", sort: (s) => s.quality.loss_avg_percent, cell: (s) => fmtPercent(locale, s.quality.loss_avg_percent) },
    { key: "poor", header: t("analytics.poorSeconds"), align: "right", sort: (s) => s.quality.poor_seconds, cell: (s) => fmtDuration(locale, s.quality.poor_seconds) },
    {
      key: "alerts",
      header: t("analytics.alerts"),
      align: "right",
      sort: (s) => s.quality.mos_alerts,
      cell: (s) => (s.quality.mos_alerts > 0 ? <span className="text-danger tabular">{s.quality.mos_alerts}</span> : <span className="text-fg-faint">0</span>),
    },
    { key: "bars", header: t("analytics.bars"), cell: (s) => <Bars bars={s.quality.bars} /> },
    { key: "node", header: t("common.node"), cell: (s) => (s.media_node_id ? <Mono title={s.media_node_id}>{shortId(s.media_node_id)}</Mono> : "—") },
    { key: "connected", header: t("analytics.connected"), sort: (s) => s.connected_at, cell: (s) => <span title={fmtDateTime(locale, s.connected_at)}>{fmtRelative(locale, s.connected_at, now)}</span> },
  ];

  const table = (
    <Card>
      <CardHeader title={t("analytics.worstSessions")} description={t("analytics.worstSessions.desc")} />
      <Toolbar
        end={
          <Button variant="ghost" size="icon" onClick={() => void worst.refetch()} aria-label={t("common.refresh")} disabled={worst.isFetching}>
            <RefreshCw className={worst.isFetching ? "size-4 animate-spin" : "size-4"} />
          </Button>
        }
      >
        <NativeSelect value={String(minSamples)} onChange={(e) => setMinSamples(Number(e.target.value))} className="h-8 w-auto">
          {MIN_SAMPLES.map((n) => (
            <option key={n} value={n}>
              {t("analytics.minSamples", { n })}
            </option>
          ))}
        </NativeSelect>
        <NativeSelect value={String(limit)} onChange={(e) => setLimit(Number(e.target.value))} className="h-8 w-auto">
          {LIMITS.map((n) => (
            <option key={n} value={n}>
              {t("analytics.top", { n })}
            </option>
          ))}
        </NativeSelect>
        {worst.data && worst.data.min_samples !== minSamples ? <span className="text-xs text-fg-faint">{t("analytics.minSamplesApplied", { n: worst.data.min_samples })}</span> : null}
      </Toolbar>
      <DataTable
        rows={sessions}
        columns={columns}
        rowKey={(s) => s.session_id}
        loading={worst.isPending}
        error={worst.isError ? errorMessage(worst.error, locale) : undefined}
        selectedKey={selected}
        onRowClick={(s) => setSelected(s.session_id === selected ? null : s.session_id)}
        dense
        empty={<EmptyState compact icon={<BarChart3 className="size-5" />} title={t("analytics.worstSessions.empty")} description={t("analytics.worstSessions.empty.desc")} />}
      />
    </Card>
  );

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 lg:grid-cols-5 gap-3">
        <Stat label={t("analytics.mos")} value={usage.isPending ? <SK /> : <MosBadge mos={agg?.mos_avg} />} hint={agg ? t("analytics.samplesCount", { n: fmtNumber(locale, agg.samples) }) : undefined} />
        <Stat label={t("analytics.rtt")} value={usage.isPending ? <SK /> : ms(locale, agg?.rtt_avg_ms)} />
        <Stat label={t("analytics.jitter")} value={usage.isPending ? <SK /> : ms(locale, agg?.jitter_avg_ms, 1)} />
        <Stat label={t("analytics.loss")} value={usage.isPending ? <SK /> : fmtPercent(locale, agg?.loss_avg_percent)} tone={agg && (agg.loss_avg_percent ?? 0) >= 5 ? "warn" : undefined} />
        <Stat
          label={t("analytics.poor")}
          value={usage.isPending ? <SK /> : fmtPercent(locale, agg?.poor_percent)}
          hint={agg ? t("analytics.poorCount", { n: fmtNumber(locale, agg.poor_samples) }) : undefined}
          tone={agg && (agg.poor_percent ?? 0) >= 10 ? "danger" : undefined}
        />
      </div>
      <div className="grid lg:grid-cols-2 gap-4">
        <ChartCard title={t("analytics.qualityChart")} description={t("analytics.qualityChart.desc")} series={qualitySeries} query={usage}>
          <TimeSeriesChart data={quality} series={qualitySeries} leftDomain={[1, 4.5]} rightDomain={[0, "auto"]} empty={t("common.empty")} />
        </ChartCard>
        <ChartCard title={t("analytics.latencyChart")} description={t("analytics.latencyChart.desc")} series={latencySeries} query={usage}>
          <TimeSeriesChart data={latency} series={latencySeries} empty={t("common.empty")} />
        </ChartCard>
      </div>
      {sel ? <SplitLayout main={table} side={<SessionCard s={sel} onClose={() => setSelected(null)} />} /> : table}
    </div>
  );
}

const CHANNEL_SORTS: readonly ChannelSortKey[] = ["participant_minutes", "peak_participants", "joins", "unique_users", "chat_messages"];

function ChannelCard({ channelId, name, range, onClose }: { channelId: string; name: string | null; range: Range; onClose: () => void }) {
  const { t, locale } = useI18n();
  const usage = useChannelUsageQuery(channelId, { from: range.from, to: range.to });
  const series = usage.data?.series;
  const points = useMemo(() => channelPoints(series ?? []), [series]);
  const defs: SeriesDef[] = [
    { key: "participants", label: t("analytics.peakParticipants"), color: chart.accent, area: true },
    { key: "joins", label: t("analytics.joins"), color: chart.faint },
    { key: "minutes", label: t("analytics.participantMinutes"), color: chart.muted, right: true, format: (v) => fmtMinutes(locale, v) },
  ];
  const tot = usage.data?.totals;
  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-2 min-w-0">
              {name ? <span className="truncate">{name}</span> : null}
              <ChannelRef id={channelId} />
            </span>
          }
          actions={
            <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
              <X className="size-4" />
            </Button>
          }
        />
        <div className="px-4 pb-4">
          {usage.isError ? (
            <QueryError error={usage.error} onRetry={() => void usage.refetch()} compact />
          ) : tot ? (
            <KV
              cols={2}
              items={[
                { k: t("analytics.peakParticipants"), v: fmtNumber(locale, tot.peak_participants) },
                { k: t("analytics.participantMinutes"), v: fmtMinutes(locale, tot.participant_minutes) },
                { k: t("analytics.joins"), v: fmtNumber(locale, tot.joins) },
                { k: t("analytics.chatMessages"), v: fmtNumber(locale, tot.chat_messages) },
                { k: t("analytics.tts"), v: fmtNumber(locale, tot.tts_requests) },
                { k: t("analytics.stt"), v: fmtMinutes(locale, tot.stt_audio_ms / 60_000) },
              ]}
            />
          ) : (
            <Skeleton className="h-24" />
          )}
        </div>
      </Card>
      <ChartCard title={t("analytics.channelChart")} description={t("analytics.channelChart.desc")} series={defs} query={usage}>
        <TimeSeriesChart data={points} series={defs} empty={t("common.empty")} />
      </ChartCard>
    </div>
  );
}

function ChannelsTab({ range }: { range: Range }) {
  const { t, locale } = useI18n();
  const list = useChannelUsageListQuery({ from: range.from, to: range.to });
  const channels = useChannelsQuery({ page: 1, per_page: 200, active_only: false }, false);
  const [sortKey, setSortKey] = useState<ChannelSortKey>("participant_minutes");
  const [selected, setSelected] = useState<string | null>(null);
  const nameOf = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of channels.data?.data ?? []) m.set(c.id, c.name);
    return (id: string) => m.get(id) ?? null;
  }, [channels.data]);
  const rows = useMemo(() => rankChannels(list.data?.channels ?? [], sortKey), [list.data, sortKey]);
  const max = rows[0]?.[sortKey] ?? 0;

  const columns: Column<T.ChannelUsageTotals>[] = [
    {
      key: "channel",
      header: t("common.channel"),
      cell: (c) => {
        const name = nameOf(c.channel_id);
        return (
          <span className="inline-flex items-center gap-1.5 min-w-0">
            {name ? <span className="truncate font-medium">{name}</span> : null}
            <ChannelRef id={c.channel_id} />
          </span>
        );
      },
    },
    {
      key: "share",
      header: "",
      width: "8rem",
      cell: (c) => (
        <div className="h-1.5 w-full rounded-full bg-surface-2 overflow-hidden">
          <div className="h-full bg-fg/70" style={{ width: `${max > 0 ? (c[sortKey] / max) * 100 : 0}%` }} />
        </div>
      ),
    },
    { key: "participant_minutes", header: t("analytics.participantMinutes"), align: "right", sort: (c) => c.participant_minutes, cell: (c) => fmtMinutes(locale, c.participant_minutes) },
    { key: "peak_participants", header: t("analytics.peakParticipants"), align: "right", sort: (c) => c.peak_participants, cell: (c) => fmtNumber(locale, c.peak_participants) },
    { key: "joins", header: t("analytics.joins"), align: "right", sort: (c) => c.joins, cell: (c) => fmtNumber(locale, c.joins) },
    { key: "unique_users", header: t("analytics.uniqueUsers"), align: "right", sort: (c) => c.unique_users, cell: (c) => fmtNumber(locale, c.unique_users) },
    { key: "chat_messages", header: t("analytics.chatMessages"), align: "right", sort: (c) => c.chat_messages, cell: (c) => fmtNumber(locale, c.chat_messages) },
    { key: "tts", header: t("analytics.tts"), align: "right", sort: (c) => c.tts_requests, cell: (c) => fmtNumber(locale, c.tts_requests) },
    { key: "stt", header: t("analytics.stt"), align: "right", sort: (c) => c.stt_audio_ms, cell: (c) => fmtMinutes(locale, c.stt_audio_ms / 60_000) },
  ];

  const table = (
    <Card>
      <CardHeader title={t("analytics.channels")} description={t("analytics.channels.desc")} />
      <Toolbar
        end={
          <Button variant="ghost" size="icon" onClick={() => void list.refetch()} aria-label={t("common.refresh")} disabled={list.isFetching}>
            <RefreshCw className={list.isFetching ? "size-4 animate-spin" : "size-4"} />
          </Button>
        }
      >
        <span className="text-xs text-fg-muted">{t("analytics.rankBy")}</span>
        <NativeSelect value={sortKey} onChange={(e) => setSortKey(CHANNEL_SORTS.find((k) => k === e.target.value) ?? "participant_minutes")} className="h-8 w-auto">
          {CHANNEL_SORTS.map((k) => (
            <option key={k} value={k}>
              {t(`analytics.rank.${k}`)}
            </option>
          ))}
        </NativeSelect>
        {list.data ? <span className="text-xs text-fg-faint">{t("common.count.items", { n: list.data.channels.length })}</span> : null}
      </Toolbar>
      <DataTable
        rows={rows}
        columns={columns}
        rowKey={(c) => c.channel_id}
        loading={list.isPending}
        error={list.isError ? errorMessage(list.error, locale) : undefined}
        selectedKey={selected}
        onRowClick={(c) => setSelected(c.channel_id === selected ? null : c.channel_id)}
        dense
        empty={<EmptyState compact icon={<BarChart3 className="size-5" />} title={t("analytics.channels.empty")} />}
      />
    </Card>
  );

  return selected ? <SplitLayout main={table} side={<ChannelCard channelId={selected} name={nameOf(selected)} range={range} onClose={() => setSelected(null)} />} /> : table;
}

function Analytics() {
  const { t } = useI18n();
  const { tab, go } = useAnalyticsSearch();
  const [range, setRange] = useRange("7d");
  const tooLong = Date.parse(range.to) - Date.parse(range.from) > 400 * 86_400_000;

  return (
    <>
      <PageHeader
        title={t("analytics.title")}
        description={t("analytics.subtitle")}
        actions={
          <div className="flex items-center gap-2 flex-wrap">
            <RangePicker value={range} onChange={setRange} />
            <ExportMenu range={range} />
          </div>
        }
        tabs={<Tabs value={tab} onValueChange={(v) => go(parseTab(v))} tabs={ANALYTICS_TABS.map((value) => ({ value, label: t(`analytics.tabs.${value}`) }))} />}
      />
      {tooLong ? (
        <Callout tone="warn" className="mb-4">
          {t("analytics.rangeTooLong")}
        </Callout>
      ) : null}
      {tab === "usage" ? <UsageTab range={range} /> : tab === "quality" ? <QualityTab range={range} /> : <ChannelsTab range={range} />}
    </>
  );
}

export default function AnalyticsPage() {
  const { canApp } = useAuth();
  return <RequireApp>{() => (canApp("analytics:read") ? <Analytics /> : <Forbidden perm="analytics:read" />)}</RequireApp>;
}
