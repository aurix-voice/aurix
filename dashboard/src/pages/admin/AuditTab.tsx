import { Link2, Link2Off, ScrollText, Sprout } from "lucide-react";
import { useMemo, useState } from "react";

import type { T } from "@/api/client";
import { useAuditLogQuery, useSelectedApp, type AuditScope } from "@/api/hooks";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime } from "@/lib/format";
import { Dialog } from "@/ui/Dialog";
import { Input, NativeSelect } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, Card, CodeBlock, CopyButton, EmptyState, IdChip, KV, Mono, Tip } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";

import { auditActions, chainLinks, chainSummary, detailsPreview, filterAudit, type ChainLink } from "./model";

const PER_PAGE = 50;

function ChainIcon({ link }: { link: ChainLink | undefined }) {
  const { t } = useI18n();
  if (link === "genesis")
    return (
      <Tip content={t("audit.chain.genesis")}>
        <Sprout className="size-3.5 text-ok" />
      </Tip>
    );
  if (link === "linked")
    return (
      <Tip content={t("audit.chain.linked")}>
        <Link2 className="size-3.5 text-fg-muted" />
      </Tip>
    );
  return (
    <Tip content={t("audit.chain.unlinked")}>
      <Link2Off className="size-3.5 text-fg-faint" />
    </Tip>
  );
}

function ActionBadge({ action }: { action: string }) {
  const tone = action.includes("deleted") || action.includes("banned") || action.includes("kicked") || action.includes("revoked") || action.includes("deactivated")
    ? "danger"
    : action.includes("created") || action.includes("login") || action.includes("unbanned") || action.includes("undrained")
      ? "ok"
      : action.includes("drained") || action.includes("muted") || action.includes("sweep")
        ? "warn"
        : "neutral";
  return (
    <Badge tone={tone}>
      <span className="font-mono text-[11px]">{action}</span>
    </Badge>
  );
}

export function AuditTab() {
  const { t, locale } = useI18n();
  const { can, canApp } = useAuth();
  const { appId } = useAppScope();
  const app = useSelectedApp();

  const platformOk = can("audit:read");
  const appOk = !!appId && canApp("audit:read");
  const [scope, setScope] = useState<AuditScope>(platformOk ? "platform" : "app");
  const effectiveScope: AuditScope = scope === "platform" && !platformOk ? "app" : scope === "app" && !appOk ? "platform" : scope;
  const [page, setPage] = useState(1);
  const [action, setAction] = useState("");
  const [actor, setActor] = useState("");
  const [target, setTarget] = useState("");
  const [selected, setSelected] = useState<T.AuditLogEntry | null>(null);

  const query = useAuditLogQuery(effectiveScope, { page, per_page: PER_PAGE });
  const entries = useMemo(() => query.data ?? [], [query.data]);
  const links = useMemo(() => chainLinks(entries), [entries]);
  const summary = useMemo(() => chainSummary(links), [links]);
  const actions = useMemo(() => auditActions(entries), [entries]);
  const rows = useMemo(() => filterAudit(entries, { action, actor, target }), [entries, action, actor, target]);

  const columns: Column<T.AuditLogEntry>[] = [
    {
      key: "time",
      header: t("common.time"),
      cell: (e) => <span className="text-fg-muted whitespace-nowrap tabular">{fmtDateTime(locale, e.created_at)}</span>,
    },
    { key: "action", header: t("audit.action"), cell: (e) => <ActionBadge action={e.action} /> },
    { key: "actor", header: t("audit.actor"), cell: (e) => <IdChip id={e.actor_id} /> },
    {
      key: "target",
      header: t("audit.target"),
      cell: (e) => (
        <span className="inline-flex items-center gap-1.5 min-w-0">
          <Badge tone="neutral">{e.target_type}</Badge>
          <Mono className="truncate max-w-[14rem] text-xs" title={e.target_id}>
            {e.target_id}
          </Mono>
        </span>
      ),
    },
    {
      key: "details",
      header: t("common.details"),
      cell: (e) => <Mono className="text-xs text-fg-muted truncate max-w-[20rem] block">{detailsPreview(e.details) || "—"}</Mono>,
    },
    { key: "app", header: t("common.app"), hidden: effectiveScope === "app", cell: (e) => (e.app_id ? <IdChip id={e.app_id} /> : <span className="text-fg-faint">{t("audit.scope.platform")}</span>) },
    { key: "ip", header: t("audit.ip"), cell: (e) => (e.ip_address ? <Mono className="text-xs">{e.ip_address}</Mono> : <span className="text-fg-faint">—</span>) },
    {
      key: "hash",
      header: t("audit.hash"),
      align: "right",
      cell: (e) => (
        <span className="inline-flex items-center gap-1.5 justify-end">
          <ChainIcon link={links.get(e.id)} />
          <Mono className="text-xs text-fg-muted" title={e.hash}>
            {e.hash.slice(0, 10)}
          </Mono>
        </span>
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      <Toolbar
        end={
          platformOk && appOk ? (
            <Segmented<AuditScope>
              size="sm"
              value={effectiveScope}
              onChange={(v) => {
                setScope(v);
                setPage(1);
              }}
              options={[
                { value: "platform", label: t("audit.scope.platform") },
                { value: "app", label: app ? app.name : t("audit.scope.app") },
              ]}
            />
          ) : null
        }
      >
        <NativeSelect value={action} onChange={(e) => setAction(e.target.value)} className="w-56" aria-label={t("audit.filter.action")}>
          <option value="">{t("audit.filter.action")}: {t("common.all")}</option>
          {actions.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
        </NativeSelect>
        <Input value={actor} onChange={(e) => setActor(e.target.value)} placeholder={t("audit.filter.actor")} className="w-44 font-mono text-xs" />
        <Input value={target} onChange={(e) => setTarget(e.target.value)} placeholder={t("audit.filter.target")} className="w-44 font-mono text-xs" />
      </Toolbar>

      {entries.length ? (
        <Callout tone={summary.unlinked === 0 ? "ok" : "neutral"} title={t("audit.chain.summary", { linked: summary.linked + summary.genesis, total: summary.total, starts: summary.genesis })}>
          {t("audit.chain.hint")}
        </Callout>
      ) : null}

      <Card>
        <DataTable
          rows={rows}
          columns={columns}
          rowKey={(e) => e.id}
          loading={query.isPending}
          dense
          onRowClick={setSelected}
          selectedKey={selected?.id ?? null}
          error={query.isError ? <QueryError error={query.error} onRetry={() => void query.refetch()} compact /> : undefined}
          empty={<EmptyState compact icon={<ScrollText className="size-5" />} title={entries.length ? t("common.noResults") : t("audit.empty")} />}
          footer={
            <Pager
              page={page}
              hasPrev={page > 1}
              hasNext={entries.length >= PER_PAGE}
              onPage={(d) => setPage((p) => Math.max(1, p + d))}
              total={rows.length}
              totalLabel={t("common.count.items", { n: rows.length })}
            />
          }
        />
      </Card>

      <Dialog open={!!selected} onOpenChange={(o) => !o && setSelected(null)} title={selected?.action} description={selected ? fmtDateTime(locale, selected.created_at) : undefined} size="lg">
        {selected ? (
          <div className="flex flex-col gap-4">
            <KV
              cols={2}
              items={[
                { k: t("audit.actor"), v: <Mono className="text-xs">{selected.actor_id}</Mono> },
                { k: t("audit.targetType"), v: selected.target_type },
                { k: t("audit.target"), v: <Mono className="text-xs">{selected.target_id}</Mono> },
                { k: t("common.app"), v: selected.app_id ? <Mono className="text-xs">{selected.app_id}</Mono> : t("audit.scope.platform") },
                { k: t("audit.ip"), v: selected.ip_address ?? "—" },
                { k: t("common.id"), v: <Mono className="text-xs">{selected.id}</Mono> },
                {
                  k: t("audit.hash"),
                  v: (
                    <span className="inline-flex items-center gap-1 min-w-0">
                      <ChainIcon link={links.get(selected.id)} />
                      <Mono className="text-xs truncate" title={selected.hash}>
                        {selected.hash}
                      </Mono>
                      <CopyButton value={selected.hash} />
                    </span>
                  ),
                },
                {
                  k: t("audit.previousHash"),
                  v: (
                    <span className="inline-flex items-center gap-1 min-w-0">
                      <Mono className="text-xs truncate" title={selected.previous_hash}>
                        {selected.previous_hash}
                      </Mono>
                      <CopyButton value={selected.previous_hash} />
                    </span>
                  ),
                },
              ]}
            />
            <div>
              <div className="text-[11px] uppercase tracking-wide text-fg-faint font-medium mb-1.5">{t("common.details")}</div>
              <CodeBlock value={JSON.stringify(selected.details ?? null, null, 2)} maxHeight="20rem" />
            </div>
          </div>
        ) : null}
      </Dialog>
    </div>
  );
}
