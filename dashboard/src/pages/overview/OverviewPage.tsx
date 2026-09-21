import { Link, useNavigate } from "@tanstack/react-router";
import { Activity, AlertTriangle, Boxes, CheckCircle2 } from "lucide-react";
import { useMemo, type ReactNode } from "react";

import type { T } from "@/api/client";
import {
  useAppAnalyticsQuery,
  useAppsQuery,
  useEventSnapshotQuery,
  useFleetUsageQuery,
  useModerationEventsQuery,
  useNodesQuery,
  useQuotaQuery,
  useSelectedApp,
  useWebhooksQuery,
} from "@/api/hooks";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { chart, ChartCard, MosBadge, mosTone, QuotaRow, TimeSeriesChart, type Point, type SeriesDef } from "@/components/charts";
import { RangePicker, stepFor, useRange } from "@/components/RangePicker";
import { useI18n } from "@/i18n";
import { fmtCompact, fmtMos, fmtNumber, fmtPercent, fmtRelative, shortId } from "@/lib/format";
import { PageHeader, QueryError } from "@/ui/Page";
import { Badge, Card, CardHeader, EmptyState, IdChip, Progress, Skeleton, Stat, type Tone } from "@/ui/Primitives";
import { DataTable, type Column } from "@/ui/Table";

interface Alert {
  key: string;
  tone: Tone;
  text: string;
  to?: "/nodes" | "/apps" | "/analytics" | "/moderation";
}

function nodeLoad(n: T.MediaNode): number | null {
  if (!n.capacity) return null;
  return ((n.active_participants ?? 0) / n.capacity) * 100;
}

function OpenLink({ to, params, search }: { to: "/nodes" | "/live" | "/moderation" | "/analytics" | "/apps/$appId"; params?: { appId: string }; search?: Record<string, string | undefined> }) {
  const { t } = useI18n();
  return (
    <Link to={to} params={params} search={search ?? {}} className="text-xs text-fg-muted hover:text-fg shrink-0">
      {t("common.open")} →
    </Link>
  );
}

function CardBody({ children }: { children: ReactNode }) {
  return <div className="px-4 pb-4 text-sm text-fg-muted">{children}</div>;
}

function Rows() {
  return (
    <div className="flex flex-col gap-2 px-4 pb-4">
      <Skeleton className="h-8" />
      <Skeleton className="h-8" />
    </div>
  );
}

export default function OverviewPage() {
  const { t, locale } = useI18n();
  const { can, canApp } = useAuth();
  const { setAppId } = useAppScope();
  const navigate = useNavigate();
  const [range, setRange] = useRange("7d");
  const step = stepFor(range);

  const nodes = useNodesQuery();
  const apps = useAppsQuery();
  const fleet = useFleetUsageQuery({ from: range.from, to: range.to });
  const app = useSelectedApp();
  const analytics = useAppAnalyticsQuery({ from: range.from, to: range.to, step });
  const quota = useQuotaQuery();
  const snapshot = useEventSnapshotQuery();
  const moderation = useModerationEventsQuery({ status: "pending", page: 1, per_page: 6 });
  const webhooks = useWebhooksQuery();

  const fleetTotals = useMemo(() => {
    const list = nodes.data ?? [];
    return {
      total: list.length,
      healthy: list.filter((n) => n.healthy).length,
      draining: list.filter((n) => !!n.drain).length,
      sessions: list.reduce((a, n) => a + (n.active_participants ?? 0), 0),
      channels: list.reduce((a, n) => a + (n.active_channels ?? 0), 0),
    };
  }, [nodes.data]);

  const usageTotals = useMemo(() => {
    const items = fleet.data?.apps ?? [];
    const samples = items.reduce((a, x) => a + x.quality_samples, 0);
    const poor = items.reduce((a, x) => a + x.poor_quality_samples, 0);
    return {
      minutes: items.reduce((a, x) => a + x.participant_minutes, 0),
      mos: samples > 0 ? items.reduce((a, x) => a + x.mos_sum_milli, 0) / samples / 1000 : null,
      poorPct: samples > 0 ? (poor / samples) * 100 : null,
    };
  }, [fleet.data]);

  const appName = useMemo(() => {
    const m = new Map<string, T.App>();
    for (const a of apps.data ?? []) m.set(a.id, a);
    return (id: string) => m.get(id)?.name ?? shortId(id);
  }, [apps.data]);

  const alerts = useMemo<Alert[]>(() => {
    const out: Alert[] = [];
    for (const n of nodes.data ?? []) {
      if (!n.healthy) {
        out.push({ key: `down:${n.id}`, tone: "danger", to: "/nodes", text: t("overview.alert.nodeDown", { node: n.id, region: n.region, when: fmtRelative(locale, n.last_heartbeat) }) });
      } else if (n.drain) {
        out.push({ key: `drain:${n.id}`, tone: "warn", to: "/nodes", text: t("overview.alert.nodeDraining", { node: n.id, region: n.region, reason: n.drain.reason ?? undefined }) });
      }
      const load = nodeLoad(n);
      if (load !== null && load >= 85) out.push({ key: `load:${n.id}`, tone: "warn", to: "/nodes", text: t("overview.alert.nodeLoad", { node: n.id, pct: Math.round(load) }) });
    }
    if (app && quota.data && quota.data.monthly_participant_minutes > 0) {
      const pct = (quota.data.participant_minutes_this_month / quota.data.monthly_participant_minutes) * 100;
      if (pct >= 80) out.push({ key: "quota", tone: pct >= 100 ? "danger" : "warn", to: "/apps", text: t("overview.alert.quota", { app: app.name, pct: Math.round(pct) }) });
    }
    for (const w of webhooks.data?.webhooks ?? []) {
      if (w.enabled && w.consecutive_failures >= 3) out.push({ key: `wh:${w.id}`, tone: "warn", to: "/apps", text: t("overview.alert.webhookFailing", { url: w.url, n: w.consecutive_failures }) });
    }
    const q = analytics.data?.totals.quality;
    if (q && q.poor_samples > 0 && (q.poor_percent ?? 0) >= 10) out.push({ key: "mos", tone: "warn", to: "/analytics", text: t("overview.alert.mos", { n: q.poor_samples }) });
    return out;
  }, [nodes.data, quota.data, webhooks.data, analytics.data, app, t, locale]);

  const usagePoints = useMemo<Point[]>(
    () => (analytics.data?.series ?? []).map((b) => ({ t: Date.parse(b.bucket), peak: b.peak_sessions, minutes: b.participant_minutes })),
    [analytics.data],
  );
  const qualityPoints = useMemo<Point[]>(
    () => (analytics.data?.series ?? []).map((b) => ({ t: Date.parse(b.bucket), mos: b.quality.mos_avg, loss: b.quality.loss_avg_percent })),
    [analytics.data],
  );
  const usageSeries: SeriesDef[] = [
    { key: "peak", label: t("overview.peakSessions"), color: chart.accent, area: true },
    { key: "minutes", label: t("overview.participantMinutes"), color: chart.muted, right: true },
  ];
  const qualitySeries: SeriesDef[] = [
    { key: "mos", label: t("overview.mos"), color: chart.ok, format: fmtMos },
    { key: "loss", label: t("overview.loss"), color: chart.danger, right: true, format: (v) => fmtPercent(locale, v) },
  ];

  const appRows = useMemo(() => [...(fleet.data?.apps ?? [])].sort((a, b) => b.participant_minutes - a.participant_minutes).slice(0, 8), [fleet.data]);
  const appColumns: Column<T.FleetUsageAppsItem>[] = [
    { key: "app", header: t("common.name"), cell: (r) => <span className="font-medium">{appName(r.app_id)}</span> },
    { key: "peak", header: t("overview.peakSessions"), align: "right", cell: (r) => fmtNumber(locale, r.peak_sessions) },
    { key: "minutes", header: t("overview.participantMinutes"), align: "right", cell: (r) => fmtCompact(locale, r.participant_minutes) },
    { key: "mos", header: t("overview.mos"), align: "right", cell: (r) => <MosBadge mos={r.quality.mos_avg} /> },
    { key: "poor", header: t("overview.poor"), align: "right", cell: (r) => (r.quality.poor_percent === null ? "—" : fmtPercent(locale, r.quality.poor_percent)) },
  ];

  const regions = useMemo(() => {
    const m = new Map<string, T.MediaNode[]>();
    for (const n of nodes.data ?? []) m.set(n.region, [...(m.get(n.region) ?? []), n]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [nodes.data]);

  const liveChannels = useMemo(
    () =>
      (snapshot.data?.channels ?? [])
        .filter((c) => (c.participants?.length ?? 0) > 0)
        .sort((a, b) => (b.participants?.length ?? 0) - (a.participants?.length ?? 0))
        .slice(0, 8),
    [snapshot.data],
  );

  const showAnalytics = can("analytics:read");
  const skel = (w: string) => <Skeleton className={`h-7 ${w}`} />;

  return (
    <div className="flex flex-col gap-5">
      <PageHeader title={t("overview.title")} description={t("overview.subtitle")} actions={<RangePicker value={range} onChange={setRange} />} />

      {nodes.isError ? <QueryError error={nodes.error} onRetry={() => void nodes.refetch()} compact /> : null}

      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
        <Stat label={t("overview.ccu")} value={nodes.isPending ? skel("w-14") : fmtNumber(locale, fleetTotals.sessions)} hint={t("overview.ccu.desc")} />
        <Stat label={t("overview.channels")} value={nodes.isPending ? skel("w-14") : fmtNumber(locale, fleetTotals.channels)} />
        <Stat
          label={t("overview.nodes")}
          value={nodes.isPending ? skel("w-14") : fmtNumber(locale, fleetTotals.total)}
          tone={fleetTotals.total > 0 && fleetTotals.healthy < fleetTotals.total ? "danger" : undefined}
          hint={
            <>
              {t("overview.nodes.healthy", { healthy: fleetTotals.healthy, total: fleetTotals.total })}
              {fleetTotals.draining > 0 ? ` · ${t("overview.nodes.draining", { n: fleetTotals.draining })}` : ""}
            </>
          }
        />
        <Stat label={t("overview.minutes")} value={showAnalytics && fleet.isPending ? skel("w-16") : fmtCompact(locale, usageTotals.minutes)} hint={t("overview.minutes.desc")} />
        <Stat label={t("overview.mos")} value={showAnalytics && fleet.isPending ? skel("w-12") : fmtMos(usageTotals.mos)} tone={mosTone(usageTotals.mos)} hint={t("overview.mos.desc")} />
        <Stat
          label={t("overview.poor")}
          value={showAnalytics && fleet.isPending ? skel("w-12") : usageTotals.poorPct === null ? "—" : fmtPercent(locale, usageTotals.poorPct)}
          tone={usageTotals.poorPct !== null && usageTotals.poorPct >= 10 ? "warn" : undefined}
          hint={t("overview.poor.desc")}
        />
      </div>

      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-2">
              {alerts.length ? <AlertTriangle className="size-4 text-warn" /> : <CheckCircle2 className="size-4 text-ok" />}
              {t("overview.alerts")}
            </span>
          }
        />
        {alerts.length === 0 ? (
          <CardBody>{t("overview.alerts.none")}</CardBody>
        ) : (
          <ul className="divide-y divide-border">
            {alerts.map((a) => (
              <li key={a.key} className="flex items-center gap-3 px-4 py-2.5 text-sm">
                <span className={`size-2 shrink-0 rounded-full ${a.tone === "danger" ? "bg-danger" : "bg-warn"}`} />
                <span className="min-w-0 flex-1 truncate">{a.text}</span>
                {a.to ? <OpenLink to={a.to === "/apps" ? (app ? "/apps/$appId" : "/nodes") : a.to} params={app ? { appId: app.id } : undefined} /> : null}
              </li>
            ))}
          </ul>
        )}
      </Card>

      {!app ? (
        <EmptyState compact icon={<Boxes className="size-5" />} title={t("common.selectApp")} description={t("common.selectApp.desc")} />
      ) : !canApp("analytics:read") ? (
        <EmptyState compact icon={<Activity className="size-5" />} title={t("overview.noAnalytics")} />
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          <ChartCard title={t("overview.usageChart")} description={`${app.name} · ${t("overview.usageChart.desc")}`} series={usageSeries} query={analytics}>
            <TimeSeriesChart data={usagePoints} series={usageSeries} empty={t("common.empty")} />
          </ChartCard>
          <ChartCard title={t("overview.qualityChart")} description={`${app.name} · ${t("overview.qualityChart.desc")}`} series={qualitySeries} query={analytics}>
            <TimeSeriesChart data={qualityPoints} series={qualitySeries} leftDomain={[1, 4.5]} rightDomain={[0, "auto"]} empty={t("common.empty")} />
          </ChartCard>
        </div>
      )}

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="xl:col-span-2">
          <CardHeader title={t("overview.topApps")} description={t("overview.topApps.desc")} />
          {showAnalytics ? (
            <DataTable
              rows={appRows}
              columns={appColumns}
              rowKey={(r) => r.app_id}
              loading={fleet.isPending}
              dense
              error={fleet.isError ? <QueryError error={fleet.error} onRetry={() => void fleet.refetch()} compact /> : undefined}
              empty={<EmptyState compact title={t("common.empty")} />}
              selectedKey={app?.id ?? null}
              onRowClick={(r) => {
                setAppId(r.app_id);
                void navigate({ to: "/apps/$appId", params: { appId: r.app_id }, search: { tab: "usage" } });
              }}
            />
          ) : (
            <EmptyState compact title={t("overview.noAnalytics")} />
          )}
        </Card>

        <Card>
          <CardHeader title={t("overview.fleet")} description={t("overview.fleet.desc")} actions={<OpenLink to="/nodes" />} />
          {nodes.isPending ? (
            <Rows />
          ) : regions.length === 0 ? (
            <CardBody>{t("nodes.empty")}</CardBody>
          ) : (
            <ul className="divide-y divide-border">
              {regions.map(([region, list]) => {
                const cap = list.reduce((a, n) => a + (n.capacity ?? 0), 0);
                const used = list.reduce((a, n) => a + (n.active_participants ?? 0), 0);
                const bad = list.filter((n) => !n.healthy).length;
                const draining = list.filter((n) => !!n.drain).length;
                return (
                  <li key={region} className="px-4 py-2.5">
                    <div className="flex items-center justify-between gap-2 text-sm">
                      <span className="font-medium">{region}</span>
                      <span className="flex items-center gap-1.5">
                        {bad > 0 ? (
                          <Badge tone="danger" dot>
                            {t("nodes.unhealthy")} {bad}
                          </Badge>
                        ) : null}
                        {draining > 0 ? (
                          <Badge tone="warn">
                            {t("nodes.draining")} {draining}
                          </Badge>
                        ) : null}
                        <span className="text-xs text-fg-muted tabular">{t("common.count.nodes", { n: list.length })}</span>
                      </span>
                    </div>
                    <div className="mt-1.5 flex items-center gap-2">
                      <Progress value={used} max={cap} tone={cap > 0 && used / cap >= 0.85 ? "warn" : undefined} className="flex-1" />
                      <span className="w-24 text-right text-xs text-fg-faint tabular">{cap > 0 ? `${fmtNumber(locale, used)} / ${fmtNumber(locale, cap)}` : fmtNumber(locale, used)}</span>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </Card>
      </div>

      {app ? (
        <div className="grid gap-4 xl:grid-cols-3">
          <Card>
            <CardHeader title={t("overview.selectedApp")} description={`${app.name} · ${t("overview.selectedApp.desc")}`} actions={<OpenLink to="/apps/$appId" params={{ appId: app.id }} />} />
            {!canApp("analytics:read") ? (
              <CardBody>{t("overview.noAnalytics")}</CardBody>
            ) : quota.isPending ? (
              <Rows />
            ) : quota.isError ? (
              <QueryError error={quota.error} onRetry={() => void quota.refetch()} compact />
            ) : (
              <div className="flex flex-col gap-4 px-4 pb-4">
                <QuotaRow label={t("overview.quota.ccu")} used={quota.data.active_sessions} limit={quota.data.max_concurrent_sessions} />
                <QuotaRow label={t("overview.quota.minutes")} used={quota.data.participant_minutes_this_month} limit={quota.data.monthly_participant_minutes} minutes />
              </div>
            )}
          </Card>

          <Card>
            <CardHeader title={t("overview.liveNow")} description={t("overview.liveNow.desc")} actions={<OpenLink to="/live" />} />
            {!canApp("events:read") ? (
              <CardBody>{t("common.forbidden.perm", { perm: "events:read" })}</CardBody>
            ) : snapshot.isPending ? (
              <Rows />
            ) : snapshot.isError ? (
              <QueryError error={snapshot.error} onRetry={() => void snapshot.refetch()} compact />
            ) : liveChannels.length === 0 ? (
              <CardBody>{t("overview.liveNow.empty")}</CardBody>
            ) : (
              <ul className="divide-y divide-border">
                {liveChannels.map((c, i) => (
                  <li key={c.channel_id ?? i} className="flex items-center gap-3 px-4 py-2 text-sm">
                    {c.channel_id ? (
                      <Link to="/live/$channelId" params={{ channelId: c.channel_id }} search={{}} className="min-w-0 flex-1 truncate hover:underline">
                        <IdChip id={c.channel_id} />
                      </Link>
                    ) : (
                      <span className="flex-1" />
                    )}
                    {c.channel_type ? <Badge>{c.channel_type}</Badge> : null}
                    <span className="text-xs text-fg-muted tabular">{t("common.count.participants", { n: c.participants?.length ?? 0 })}</span>
                  </li>
                ))}
              </ul>
            )}
          </Card>

          <Card>
            <CardHeader title={t("overview.recentIncidents")} actions={<OpenLink to="/moderation" />} />
            {!canApp("moderation:read") ? (
              <CardBody>{t("common.forbidden.perm", { perm: "moderation:read" })}</CardBody>
            ) : moderation.isPending ? (
              <Rows />
            ) : moderation.isError ? (
              <QueryError error={moderation.error} onRetry={() => void moderation.refetch()} compact />
            ) : moderation.data.length === 0 ? (
              <CardBody>{t("overview.alerts.none")}</CardBody>
            ) : (
              <ul className="divide-y divide-border">
                {moderation.data.map((e) => (
                  <li key={e.id} className="px-4 py-2 text-sm">
                    <Link to="/moderation" search={{ event: e.id }} className="flex items-center gap-2 hover:underline">
                      <Badge tone="warn">{e.event_type}</Badge>
                      <span className="min-w-0 flex-1 truncate">{e.reason}</span>
                      <span className="shrink-0 text-xs text-fg-faint">{fmtRelative(locale, e.created_at)}</span>
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </div>
      ) : null}
    </div>
  );
}

