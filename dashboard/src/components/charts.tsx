import type { ReactNode } from "react";
import { Area, AreaChart, CartesianGrid, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis, type TooltipContentProps } from "recharts";
import type { NameType, ValueType } from "recharts/types/component/DefaultTooltipContent";

import { useI18n, type Locale } from "@/i18n";
import { fmtDateTime, fmtMinutes, fmtMos, fmtNumber, fmtTime } from "@/lib/format";
import { QueryError } from "@/ui/Page";
import { Badge, Card, CardHeader, Progress, Skeleton, type Tone } from "@/ui/Primitives";

/** Chart palette from the design tokens so charts follow the theme. */
export const chart = {
  fg: "var(--fg)",
  muted: "var(--fg-muted)",
  faint: "var(--fg-faint)",
  grid: "var(--border)",
  accent: "var(--accent)",
  ok: "var(--ok)",
  warn: "var(--warn)",
  danger: "var(--danger)",
} as const;

export interface SeriesDef {
  key: string;
  label: string;
  color: string;
  /** Right axis. */
  right?: boolean;
  format?: (v: number) => string;
  area?: boolean;
}

export type Point = { t: number } & Record<string, number | null>;

function tickFormatter(locale: Locale, spanMs: number) {
  return (v: number) => (spanMs > 2 * 86_400_000 ? fmtDateTime(locale, new Date(v).toISOString()).replace(/,? \d{2}:\d{2}.*$/, "") : fmtTime(locale, v));
}

function ChartTooltip({ active, payload, label, series, locale }: Partial<TooltipContentProps<ValueType, NameType>> & { series: SeriesDef[]; locale: Locale }) {
  if (!active || !payload?.length || typeof label !== "number") return null;
  return (
    <div className="rounded-md border border-border bg-surface px-2.5 py-2 text-xs shadow-sm">
      <div className="text-fg-faint mb-1">{fmtDateTime(locale, new Date(label).toISOString())}</div>
      {payload.map((p) => {
        const def = series.find((s) => s.key === p.dataKey);
        if (!def) return null;
        const v = typeof p.value === "number" ? p.value : null;
        return (
          <div key={def.key} className="flex items-center gap-2">
            <span className="size-2 rounded-full" style={{ background: def.color }} />
            <span className="text-fg-muted">{def.label}</span>
            <span className="ml-auto tabular font-medium">{v === null ? "—" : def.format ? def.format(v) : v.toLocaleString(locale)}</span>
          </div>
        );
      })}
    </div>
  );
}

export function TimeSeriesChart({
  data,
  series,
  height = 220,
  leftDomain,
  rightDomain,
  empty,
}: {
  data: Point[];
  series: SeriesDef[];
  height?: number;
  leftDomain?: [number | "auto", number | "auto"];
  rightDomain?: [number | "auto", number | "auto"];
  empty?: ReactNode;
}) {
  const { locale } = useI18n();
  if (data.length === 0) {
    return <div className="flex items-center justify-center text-xs text-fg-faint" style={{ height }}>{empty ?? "—"}</div>;
  }
  const first = data[0]?.t ?? 0;
  const last = data[data.length - 1]?.t ?? first;
  const hasRight = series.some((s) => s.right);
  const anyArea = series.some((s) => s.area);
  const Chart = anyArea ? AreaChart : LineChart;
  return (
    <ResponsiveContainer width="100%" height={height}>
      <Chart data={data} margin={{ top: 8, right: hasRight ? 4 : 12, bottom: 0, left: 0 }}>
        <CartesianGrid stroke={chart.grid} vertical={false} />
        <XAxis
          dataKey="t"
          type="number"
          domain={["dataMin", "dataMax"]}
          scale="time"
          tickFormatter={tickFormatter(locale, last - first)}
          tick={{ fontSize: 11, fill: chart.faint }}
          axisLine={false}
          tickLine={false}
          minTickGap={40}
        />
        <YAxis yAxisId="left" tick={{ fontSize: 11, fill: chart.faint }} axisLine={false} tickLine={false} width={44} domain={leftDomain} allowDecimals={false} />
        {hasRight ? (
          <YAxis yAxisId="right" orientation="right" tick={{ fontSize: 11, fill: chart.faint }} axisLine={false} tickLine={false} width={44} domain={rightDomain} />
        ) : null}
        <Tooltip content={(p) => <ChartTooltip {...p} series={series} locale={locale} />} cursor={{ stroke: chart.grid }} />
        {series.map((s) =>
          s.area ? (
            <Area
              key={s.key}
              yAxisId={s.right ? "right" : "left"}
              type="monotone"
              dataKey={s.key}
              stroke={s.color}
              fill={s.color}
              fillOpacity={0.08}
              strokeWidth={1.6}
              dot={false}
              isAnimationActive={false}
              connectNulls
            />
          ) : (
            <Line
              key={s.key}
              yAxisId={s.right ? "right" : "left"}
              type="monotone"
              dataKey={s.key}
              stroke={s.color}
              strokeWidth={1.6}
              dot={false}
              isAnimationActive={false}
              connectNulls
            />
          ),
        )}
      </Chart>
    </ResponsiveContainer>
  );
}

export function Legend({ series }: { series: SeriesDef[] }) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-fg-muted">
      {series.map((s) => (
        <span key={s.key} className="inline-flex items-center gap-1.5">
          <span className="size-2 rounded-full" style={{ background: s.color }} />
          {s.label}
        </span>
      ))}
    </div>
  );
}

export function ChartCard({
  title,
  description,
  series,
  query,
  children,
}: {
  title: string;
  description: string;
  series: SeriesDef[];
  query: { isPending: boolean; isError: boolean; error: unknown; refetch: () => Promise<unknown> };
  children: ReactNode;
}) {
  return (
    <Card>
      <CardHeader title={title} description={description} actions={<Legend series={series} />} />
      <div className="px-2 pb-3">
        {query.isError ? <QueryError error={query.error} onRetry={() => void query.refetch()} compact /> : query.isPending ? <Skeleton className="mx-2 h-[220px]" /> : children}
      </div>
    </Card>
  );
}

export function mosTone(mos: number | null | undefined): Tone | undefined {
  if (mos === null || mos === undefined) return undefined;
  return mos >= 4 ? "ok" : mos >= 3.6 ? undefined : mos >= 3.1 ? "warn" : "danger";
}

export function MosBadge({ mos }: { mos: number | null | undefined }) {
  if (mos === null || mos === undefined) return <span className="text-fg-faint">—</span>;
  return <Badge tone={mosTone(mos) ?? "neutral"}>{fmtMos(mos)}</Badge>;
}

export function QuotaRow({ label, used, limit, minutes }: { label: string; used: number; limit: number; minutes?: boolean }) {
  const { t, locale } = useI18n();
  const fmt = (n: number) => (minutes ? fmtMinutes(locale, n) : fmtNumber(locale, n));
  const pct = limit > 0 ? (used / limit) * 100 : null;
  const tone: Tone | undefined = pct === null ? undefined : pct >= 100 ? "danger" : pct >= 80 ? "warn" : undefined;
  return (
    <div>
      <div className="flex items-center justify-between text-sm">
        <span className="text-fg-muted">{label}</span>
        <span className="tabular">{limit > 0 ? t("overview.quota.of", { used: fmt(used), limit: fmt(limit) }) : `${fmt(used)} · ${t("common.unlimited")}`}</span>
      </div>
      <Progress value={used} max={limit > 0 ? limit : null} tone={tone} className="mt-1.5" />
    </div>
  );
}
