import { Link } from "@tanstack/react-router";
import { RefreshCw, Server, Settings2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";

import type { T } from "@/api/client";
import { useDrainNodeMutation, useEffectiveConfigQuery, useNodesQuery, useUndrainNodeMutation } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtNumber, fmtPercent, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { ConfirmDialog, Dialog, FormDialog } from "@/ui/Dialog";
import { Input, Textarea } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { PageHeader, QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, EmptyState, Field, IdChip, KV, Mono, Progress, Stat, Tip, type Tone } from "@/ui/Primitives";
import { DataTable, sortRows, type Column, type SortState } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

type Filter = "all" | "healthy" | "unhealthy" | "draining";

const STALE_HEARTBEAT_MS = 30_000;

function loadPct(n: T.MediaNode): number | null {
  if (!n.capacity) return null;
  return ((n.active_participants ?? 0) / n.capacity) * 100;
}

function pctTone(pct: number | null | undefined): Tone | undefined {
  if (pct === null || pct === undefined) return undefined;
  return pct >= 95 ? "danger" : pct >= 85 ? "warn" : undefined;
}

function nodeState(n: T.MediaNode): { tone: Tone; key: "nodes.healthy" | "nodes.unhealthy" | "nodes.draining" } {
  if (!n.healthy) return { tone: "danger", key: "nodes.unhealthy" };
  if (n.drain) return { tone: "warn", key: "nodes.draining" };
  return { tone: "ok", key: "nodes.healthy" };
}

function Mbps({ v }: { v: number | undefined }) {
  const { locale } = useI18n();
  return <span className="tabular whitespace-nowrap">{v === undefined ? "—" : `${fmtNumber(locale, v, v < 10 ? 1 : 0)} Mbps`}</span>;
}

function Pct({ v }: { v: number | undefined }) {
  const { locale } = useI18n();
  if (v === undefined) return <span className="text-fg-faint">—</span>;
  return (
    <span className="inline-flex items-center gap-2 whitespace-nowrap">
      <Progress value={v} max={100} tone={pctTone(v)} className="w-12" />
      <span className="tabular text-xs">{fmtPercent(locale, v, 0)}</span>
    </span>
  );
}

function StateBadge({ node }: { node: T.MediaNode }) {
  const { t } = useI18n();
  const s = nodeState(node);
  return (
    <Badge tone={s.tone} dot>
      {t(s.key)}
    </Badge>
  );
}

export default function NodesPage() {
  const { t, locale } = useI18n();
  const { can } = useAuth();
  const toast = useToast();
  const now = useNow();
  const nodes = useNodesQuery();
  const config = useEffectiveConfigQuery();
  const drain = useDrainNodeMutation();
  const undrain = useUndrainNodeMutation();

  const [filter, setFilter] = useState<Filter>("all");
  const [search, setSearch] = useState("");
  const [region, setRegion] = useState<string | null>(null);
  const [sort, setSort] = useState<SortState | null>({ key: "region", dir: "asc" });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [drainTarget, setDrainTarget] = useState<T.MediaNode | null>(null);
  const [undrainTarget, setUndrainTarget] = useState<T.MediaNode | null>(null);
  const [reason, setReason] = useState("");

  const all = useMemo(() => nodes.data ?? [], [nodes.data]);
  const selfId = config.data?.node_id ?? null;
  const canDrain = can("nodes:drain");

  const summary = useMemo(() => {
    const versions = new Map<string, number>();
    for (const n of all) versions.set(n.version, (versions.get(n.version) ?? 0) + 1);
    const cap = all.reduce((a, n) => a + (n.capacity ?? 0), 0);
    const used = all.reduce((a, n) => a + (n.active_participants ?? 0), 0);
    return {
      total: all.length,
      healthy: all.filter((n) => n.healthy).length,
      unhealthy: all.filter((n) => !n.healthy).length,
      draining: all.filter((n) => !!n.drain).length,
      relay: all.filter((n) => n.relay_only).length,
      channels: all.reduce((a, n) => a + (n.active_channels ?? 0), 0),
      used,
      cap,
      versions: [...versions.entries()].sort((a, b) => b[1] - a[1]),
    };
  }, [all]);

  const regions = useMemo(() => {
    const m = new Map<string, T.MediaNode[]>();
    for (const n of all) m.set(n.region, [...(m.get(n.region) ?? []), n]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [all]);

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return all.filter((n) => {
      if (region && n.region !== region) return false;
      if (filter === "healthy" && (!n.healthy || n.drain)) return false;
      if (filter === "unhealthy" && n.healthy) return false;
      if (filter === "draining" && !n.drain) return false;
      if (!q) return true;
      return [n.id, n.region, n.address, n.address_ipv6 ?? "", n.version, n.ws_url ?? "", n.api_url ?? ""].some((s) => s.toLowerCase().includes(q));
    });
  }, [all, filter, region, search]);

  const columns: Column<T.MediaNode>[] = [
    {
      key: "id",
      header: t("common.node"),
      sort: (n) => n.id,
      cell: (n) => (
        <div className="flex flex-col gap-0.5 min-w-0">
          <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
            <IdChip id={n.id} />
            {n.id === selfId ? <Badge tone="accent">{t("nodes.self")}</Badge> : null}
            {n.relay_only ? <Badge>{t("nodes.relayOnly")}</Badge> : null}
          </span>
          <span className="text-xs text-fg-muted truncate">{n.api_url ?? n.address}</span>
        </div>
      ),
    },
    { key: "region", header: t("common.region"), sort: (n) => n.region, cell: (n) => n.region },
    {
      key: "state",
      header: t("common.status"),
      sort: (n) => (!n.healthy ? 0 : n.drain ? 1 : 2),
      cell: (n) => (
        <Tip content={n.drain ? [t("nodes.drain.since", { when: fmtDateTime(locale, n.drain.since) }), n.drain.by ? t("nodes.drain.by", { who: n.drain.by }) : null, n.drain.reason].filter(Boolean).join(" · ") : null}>
          <span>
            <StateBadge node={n} />
          </span>
        </Tip>
      ),
    },
    { key: "version", header: t("common.version"), sort: (n) => n.version, cell: (n) => <Mono>{n.version}</Mono> },
    {
      key: "load",
      header: t("nodes.load"),
      sort: (n) => loadPct(n) ?? -1,
      cell: (n) => {
        const pct = loadPct(n);
        return (
          <span className="inline-flex items-center gap-2 whitespace-nowrap">
            <Progress value={n.active_participants ?? 0} max={n.capacity} tone={pctTone(pct)} className="w-16" />
            <span className="tabular text-xs">
              {fmtNumber(locale, n.active_participants ?? 0)}
              {n.capacity ? <span className="text-fg-faint"> / {fmtNumber(locale, n.capacity)}</span> : null}
            </span>
          </span>
        );
      },
    },
    { key: "channels", header: t("nodes.channels"), align: "right", sort: (n) => n.active_channels ?? 0, cell: (n) => fmtNumber(locale, n.active_channels ?? 0) },
    {
      key: "res",
      header: `${t("nodes.cpu")} / ${t("nodes.memory")}`,
      sort: (n) => Math.max(n.cpu_usage ?? -1, n.memory_usage ?? -1),
      cell: (n) => (
        <span className="flex flex-col gap-0.5">
          <Pct v={n.cpu_usage} />
          <Pct v={n.memory_usage} />
        </span>
      ),
    },
    {
      key: "bw",
      header: t("nodes.bandwidth"),
      align: "right",
      sort: (n) => (n.bandwidth_in_mbps ?? 0) + (n.bandwidth_out_mbps ?? 0),
      cell: (n) => (
        <span className="flex flex-col gap-0.5 text-xs whitespace-nowrap">
          <span>
            <span className="text-fg-faint">{t("nodes.in")} </span>
            <Mbps v={n.bandwidth_in_mbps} />
          </span>
          <span>
            <span className="text-fg-faint">{t("nodes.out")} </span>
            <Mbps v={n.bandwidth_out_mbps} />
          </span>
        </span>
      ),
    },
    {
      key: "hb",
      header: t("nodes.heartbeat"),
      align: "right",
      sort: (n) => Date.parse(n.last_heartbeat),
      cell: (n) => {
        const age = now - Date.parse(n.last_heartbeat);
        return (
          <Tip content={fmtDateTime(locale, n.last_heartbeat)}>
            <span className={age > STALE_HEARTBEAT_MS ? "text-danger" : "text-fg-muted"}>{fmtRelative(locale, n.last_heartbeat, now)}</span>
          </Tip>
        );
      },
    },
    {
      key: "actions",
      header: "",
      align: "right",
      hidden: !canDrain,
      cell: (n) => (
        <span onClick={(e) => e.stopPropagation()}>
          {n.drain ? (
            <Button size="xs" variant="outline" onClick={() => setUndrainTarget(n)}>
              {t("nodes.undrain")}
            </Button>
          ) : (
            <Button size="xs" variant="ghost" onClick={() => setDrainTarget(n)}>
              {t("nodes.drain")}
            </Button>
          )}
        </span>
      ),
    },
  ];

  const rows = sortRows(filtered, columns, sort);
  const selected = all.find((n) => n.id === selectedId) ?? null;

  const startDrain = async () => {
    if (!drainTarget) return;
    await drain.mutateAsync({ nodeId: drainTarget.id, reason: reason.trim() || null });
    toast.ok(t("nodes.drained"), drainTarget.id);
    setReason("");
  };
  const clearDrain = async () => {
    if (!undrainTarget) return;
    await undrain.mutateAsync({ nodeId: undrainTarget.id });
    toast.ok(t("nodes.undrained"), undrainTarget.id);
  };

  if (!can("nodes:read")) {
    return <EmptyState icon={<Server className="size-5" />} title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "nodes:read" })} />;
  }

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        title={t("nodes.title")}
        description={t("nodes.subtitle")}
        actions={
          <Button variant="outline" size="sm" onClick={() => void nodes.refetch()} loading={nodes.isFetching && !nodes.isPending}>
            <RefreshCw className="size-3.5" />
            {t("common.refresh")}
          </Button>
        }
      />

      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
        <Stat label={t("overview.nodes")} value={fmtNumber(locale, summary.total)} hint={summary.relay > 0 ? `${t("nodes.relayOnly")}: ${summary.relay}` : undefined} />
        <Stat label={t("nodes.healthy")} value={fmtNumber(locale, summary.healthy)} tone={summary.healthy === summary.total && summary.total > 0 ? "ok" : undefined} />
        <Stat label={t("nodes.unhealthy")} value={fmtNumber(locale, summary.unhealthy)} tone={summary.unhealthy > 0 ? "danger" : undefined} />
        <Stat label={t("nodes.draining")} value={fmtNumber(locale, summary.draining)} tone={summary.draining > 0 ? "warn" : undefined} />
        <Stat
          label={t("nodes.sessions")}
          value={fmtNumber(locale, summary.used)}
          hint={summary.cap > 0 ? `${t("nodes.capacity")}: ${fmtNumber(locale, summary.cap)} · ${fmtPercent(locale, (summary.used / summary.cap) * 100, 0)}` : t("common.count.channels", { n: summary.channels })}
        />
        <Stat
          label={t("nodes.versions")}
          value={summary.versions.length === 0 ? "—" : summary.versions.length === 1 ? <Mono className="text-base">{summary.versions[0]?.[0]}</Mono> : fmtNumber(locale, summary.versions.length)}
          tone={summary.versions.length > 1 ? "warn" : undefined}
          hint={summary.versions.length > 1 ? summary.versions.map(([v, n]) => `${v} ×${n}`).join(", ") : undefined}
        />
      </div>

      {summary.versions.length > 1 ? <Callout tone="warn" title={t("nodes.versionSkew")}>{summary.versions.map(([v, n]) => `${v} ×${n}`).join(" · ")}</Callout> : null}

      {regions.length > 1 ? (
        <div className="flex flex-wrap gap-2">
          {regions.map(([name, list]) => {
            const cap = list.reduce((a, n) => a + (n.capacity ?? 0), 0);
            const used = list.reduce((a, n) => a + (n.active_participants ?? 0), 0);
            const bad = list.filter((n) => !n.healthy).length;
            const dr = list.filter((n) => !!n.drain).length;
            const active = region === name;
            return (
              <button
                key={name}
                type="button"
                onClick={() => setRegion(active ? null : name)}
                className={`card min-w-44 flex-1 px-3.5 py-3 text-left transition-colors hover:bg-surface-2 ${active ? "ring-1 ring-fg" : ""}`}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm font-medium">{name}</span>
                  <span className="flex items-center gap-1">
                    {bad > 0 ? <Badge tone="danger" dot>{bad}</Badge> : null}
                    {dr > 0 ? <Badge tone="warn">{dr}</Badge> : null}
                    <span className="text-xs text-fg-muted">{t("common.count.nodes", { n: list.length })}</span>
                  </span>
                </div>
                <div className="mt-2 flex items-center gap-2">
                  <Progress value={used} max={cap} tone={pctTone(cap ? (used / cap) * 100 : null)} className="flex-1" />
                  <span className="text-xs text-fg-faint tabular">{cap ? `${fmtNumber(locale, used)} / ${fmtNumber(locale, cap)}` : fmtNumber(locale, used)}</span>
                </div>
              </button>
            );
          })}
        </div>
      ) : null}

      <Card>
        <Toolbar
          end={<Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder={t("common.search")} className="w-56" aria-label={t("common.search")} />}
        >
          <Segmented
            size="sm"
            value={filter}
            onChange={setFilter}
            options={[
              { value: "all", label: `${t("nodes.filter.all")} ${summary.total}` },
              { value: "healthy", label: `${t("nodes.filter.healthy")} ${summary.healthy - summary.draining}` },
              { value: "unhealthy", label: `${t("nodes.filter.unhealthy")} ${summary.unhealthy}` },
              { value: "draining", label: `${t("nodes.filter.draining")} ${summary.draining}` },
            ]}
          />
          {region ? (
            <Button size="xs" variant="secondary" onClick={() => setRegion(null)}>
              {t("common.region")}: {region} ×
            </Button>
          ) : null}
        </Toolbar>
        <DataTable
          rows={rows}
          columns={columns}
          rowKey={(n) => n.id}
          loading={nodes.isPending}
          sort={sort}
          onSortChange={setSort}
          selectedKey={selectedId}
          onRowClick={(n) => setSelectedId(n.id)}
          rowClassName={(n) => (!n.healthy ? "bg-danger/5" : undefined)}
          error={nodes.isError ? <QueryError error={nodes.error} onRetry={() => void nodes.refetch()} compact /> : undefined}
          empty={
            all.length === 0 ? (
              <EmptyState compact icon={<Server className="size-5" />} title={t("nodes.empty")} description={t("nodes.empty.desc")} />
            ) : (
              <EmptyState compact title={t("common.noResults")} />
            )
          }
        />
      </Card>

      {can("config:read") ? (
        <Card>
          <CardHeader
            title={t("nodes.effectiveConfig")}
            description={t("nodes.effectiveConfig.desc")}
            actions={
              <Link to="/config" className="inline-flex items-center gap-1 text-xs text-fg-muted hover:text-fg">
                <Settings2 className="size-3.5" />
                {t("common.open")} →
              </Link>
            }
          />
          <div className="px-4 pb-4">
            {config.isError ? (
              <QueryError error={config.error} onRetry={() => void config.refetch()} compact />
            ) : config.data ? (
              <KV
                cols={4}
                items={[
                  { k: t("config.node"), v: <Mono>{config.data.node_id}</Mono> },
                  { k: t("common.version"), v: <Mono>{config.data.version}</Mono> },
                  { k: t("common.region"), v: config.data.region },
                  {
                    k: t("config.environment"),
                    v: (
                      <span className="inline-flex items-center gap-1.5">
                        {config.data.environment}
                        {config.data.production ? <Badge tone="accent">{t("config.production")}</Badge> : null}
                      </span>
                    ),
                  },
                ]}
              />
            ) : (
              <div className="text-sm text-fg-muted">{t("common.loading")}</div>
            )}
          </div>
        </Card>
      ) : null}

      <NodeDetails
        node={selected}
        selfId={selfId}
        onClose={() => setSelectedId(null)}
        actions={
          canDrain && selected ? (
            selected.drain ? (
              <Button variant="outline" onClick={() => setUndrainTarget(selected)}>
                {t("nodes.undrain")}
              </Button>
            ) : (
              <Button variant="danger" onClick={() => setDrainTarget(selected)}>
                {t("nodes.drain")}
              </Button>
            )
          ) : null
        }
      />

      <FormDialog
        open={!!drainTarget}
        onOpenChange={(o) => {
          if (!o) {
            setDrainTarget(null);
            setReason("");
          }
        }}
        title={t("nodes.drain.title", { node: drainTarget?.id ?? "" })}
        description={t("nodes.drain.desc")}
        submitLabel={t("nodes.drain.confirm")}
        onSubmit={startDrain}
        size="sm"
      >
        <Field label={t("nodes.drain.reason")} htmlFor="drain-reason">
          <Textarea id="drain-reason" value={reason} onChange={(e) => setReason(e.target.value)} placeholder={t("nodes.drain.reason.placeholder")} maxLength={500} autoFocus />
        </Field>
        {drainTarget && (drainTarget.active_participants ?? 0) > 0 ? (
          <Callout tone="neutral">{t("common.count.sessions", { n: drainTarget.active_participants ?? 0 })}</Callout>
        ) : null}
      </FormDialog>

      <ConfirmDialog
        open={!!undrainTarget}
        onOpenChange={(o) => {
          if (!o) setUndrainTarget(null);
        }}
        title={t("nodes.undrain.title", { node: undrainTarget?.id ?? "" })}
        description={t("nodes.undrain.desc")}
        confirmLabel={t("nodes.undrain")}
        onConfirm={clearDrain}
      />
    </div>
  );
}

function NodeDetails({ node, selfId, onClose, actions }: { node: T.MediaNode | null; selfId: string | null; onClose: () => void; actions: ReactNode }) {
  const { t, locale } = useI18n();
  if (!node) return null;
  const pct = loadPct(node);
  return (
    <Dialog
      open
      onOpenChange={(o) => {
        if (!o) onClose();
      }}
      size="lg"
      title={
        <span className="inline-flex items-center gap-2">
          <Mono className="text-sm">{node.id}</Mono>
          <StateBadge node={node} />
          {node.relay_only ? <Badge>{t("nodes.relayOnly")}</Badge> : null}
          {node.id === selfId ? <Badge tone="accent">{t("nodes.self")}</Badge> : null}
        </span>
      }
      description={`${node.region} · ${node.version}`}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            {t("common.close")}
          </Button>
          {actions}
        </>
      }
    >
      <div className="flex flex-col gap-5">
        {node.drain ? (
          <Callout tone="warn" title={t("nodes.draining")}>
            {t("nodes.drain.since", { when: fmtDateTime(locale, node.drain.since) })}
            {node.drain.by ? ` · ${t("nodes.drain.by", { who: node.drain.by })}` : ""}
            {node.drain.reason ? <div className="mt-1 text-fg">{node.drain.reason}</div> : null}
          </Callout>
        ) : null}

        <section>
          <h4 className="mb-2 text-[11px] font-medium uppercase tracking-wide text-fg-faint">{t("nodes.load")}</h4>
          <div className="mb-3 flex items-center gap-3">
            <Progress value={node.active_participants ?? 0} max={node.capacity} tone={pctTone(pct)} className="flex-1" />
            <span className="text-sm tabular">
              {fmtNumber(locale, node.active_participants ?? 0)}
              {node.capacity ? <span className="text-fg-faint"> / {fmtNumber(locale, node.capacity)}</span> : null}
            </span>
          </div>
          <KV
            cols={4}
            items={[
              { k: t("nodes.channels"), v: fmtNumber(locale, node.active_channels ?? 0) },
              { k: t("nodes.cpu"), v: <Pct v={node.cpu_usage} /> },
              { k: t("nodes.memory"), v: <Pct v={node.memory_usage} /> },
              {
                k: t("nodes.bandwidth"),
                v: (
                  <>
                    <Mbps v={node.bandwidth_in_mbps} /> <span className="text-fg-faint">{t("nodes.in")}</span> · <Mbps v={node.bandwidth_out_mbps} /> <span className="text-fg-faint">{t("nodes.out")}</span>
                  </>
                ),
              },
            ]}
          />
        </section>

        <section>
          <h4 className="mb-2 text-[11px] font-medium uppercase tracking-wide text-fg-faint">{t("nodes.addresses")}</h4>
          <KV
            cols={2}
            items={[
              { k: t("nodes.media"), v: <Mono>{node.address}:{node.media_port}</Mono> },
              { k: `${t("nodes.media")} (IPv6)`, v: node.address_ipv6 ? <Mono>[{node.address_ipv6}]:{node.media_port}</Mono> : null, hidden: !node.address_ipv6 },
              { k: t("nodes.api"), v: <Mono>{node.api_url ?? `${node.address}:${node.api_port}`}</Mono> },
              { k: t("nodes.ws"), v: node.ws_url ? <Mono>{node.ws_url}</Mono> : null },
              { k: t("nodes.cascade"), v: node.cascade_port ? <Mono>{node.address}:{node.cascade_port}</Mono> : null },
              {
                k: t("nodes.location"),
                v: node.location ? <Mono>{`${node.location.latitude.toFixed(3)}, ${node.location.longitude.toFixed(3)}`}</Mono> : null,
                hidden: !node.location,
              },
            ]}
          />
        </section>

        <KV
          cols={2}
          items={[
            { k: t("nodes.heartbeat"), v: `${fmtDateTime(locale, node.last_heartbeat)} (${fmtRelative(locale, node.last_heartbeat)})` },
            { k: t("nodes.registered"), v: node.registered_at ? fmtDateTime(locale, node.registered_at) : null },
          ]}
        />
      </div>
    </Dialog>
  );
}
