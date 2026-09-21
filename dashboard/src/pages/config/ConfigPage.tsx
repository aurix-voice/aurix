import { ChevronDown, ChevronRight, Copy, Download, RefreshCw, Search } from "lucide-react";
import { useMemo, useState } from "react";

import { downloadJson, type T } from "@/api/client";
import { useEffectiveConfigQuery } from "@/api/hooks";
import { useI18n } from "@/i18n";
import { fmtNumber } from "@/lib/format";
import { RequirePermission } from "@/shell/Guards";
import { Button } from "@/ui/Button";
import { Input } from "@/ui/Input";
import { PageHeader, QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, Card, CopyButton, EmptyState, KV, Mono, Skeleton } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { configFilename, configSections, countLeaves, countMasked, filterSections, formatLeaf, type ConfigSection } from "./model";

export default function ConfigPage() {
  return (
    <RequirePermission perm="config:read">
      <Config />
    </RequirePermission>
  );
}

function Config() {
  const { t, locale } = useI18n();
  const toast = useToast();
  const config = useEffectiveConfigQuery();
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());

  const sections = useMemo(() => (config.data ? configSections(config.data.config) : []), [config.data]);
  const visible = useMemo(() => filterSections(sections, query), [sections, query]);
  const masked = useMemo(() => countMasked(sections), [sections]);
  const searching = query.trim().length > 0;

  const toggle = (name: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });

  const copyAll = async () => {
    if (!config.data) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(config.data, null, 2));
      toast.ok(t("common.copied"));
    } catch {
      toast.error(t("common.copy"));
    }
  };

  return (
    <>
      <PageHeader
        title={t("config.title")}
        description={t("config.subtitle")}
        actions={
          <>
            <Button size="sm" variant="ghost" onClick={() => void config.refetch()} disabled={config.isFetching}>
              <RefreshCw className={config.isFetching ? "size-3.5 animate-spin" : "size-3.5"} />
              {t("common.refresh")}
            </Button>
            <Button size="sm" variant="secondary" onClick={() => void copyAll()} disabled={!config.data}>
              <Copy className="size-3.5" />
              {t("config.copyJson")}
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={!config.data}
              onClick={() => config.data && downloadJson(config.data, configFilename(config.data.node_id, config.data.version))}
            >
              <Download className="size-3.5" />
              {t("config.downloadJson")}
            </Button>
          </>
        }
      />

      <div className="flex flex-col gap-4">
        <Callout tone="neutral" title={t("config.readOnly")}>
          {t("config.masked")}
        </Callout>

        {config.isError ? <QueryError error={config.error} onRetry={() => void config.refetch()} /> : null}

        <Card className="p-4">
          {config.data ? <Meta data={config.data} masked={masked} keys={countLeaves(sections)} /> : <Skeleton className="h-12" />}
        </Card>

        <Card>
          <Toolbar
            end={
              <>
                <span className="text-xs text-fg-muted tabular">
                  {t("config.matches", { n: fmtNumber(locale, countLeaves(visible)), sections: fmtNumber(locale, visible.length) })}
                </span>
                <Button size="xs" variant="ghost" onClick={() => setCollapsed(new Set())} disabled={collapsed.size === 0}>
                  {t("config.expandAll")}
                </Button>
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => setCollapsed(new Set(sections.map((s) => s.name)))}
                  disabled={sections.length === 0 || collapsed.size === sections.length}
                >
                  {t("config.collapseAll")}
                </Button>
              </>
            }
          >
            <div className="relative">
              <Search className="size-3.5 absolute left-2.5 top-1/2 -translate-y-1/2 text-fg-faint" />
              <Input
                type="search"
                className="pl-8 w-72"
                placeholder={t("config.search")}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                aria-label={t("config.search")}
              />
            </div>
          </Toolbar>

          {config.isPending ? (
            <div className="p-4 flex flex-col gap-2">
              {Array.from({ length: 6 }, (_, i) => (
                <Skeleton key={i} className="h-8" />
              ))}
            </div>
          ) : visible.length === 0 ? (
            <EmptyState compact title={t("common.noResults")} />
          ) : (
            <div className="divide-y divide-border">
              {visible.map((s) => (
                <Section key={s.name} section={s} open={searching || !collapsed.has(s.name)} onToggle={() => toggle(s.name)} />
              ))}
            </div>
          )}
        </Card>
      </div>
    </>
  );
}

function Meta({ data, masked, keys }: { data: T.EffectiveConfig; masked: number; keys: number }) {
  const { t, locale } = useI18n();
  return (
    <KV
      cols={4}
      items={[
        { k: t("config.node"), v: <Mono title={data.node_id}>{data.node_id}</Mono> },
        { k: t("common.version"), v: <Mono>{data.version}</Mono> },
        { k: t("common.region"), v: data.region },
        {
          k: t("config.environment"),
          v: (
            <span className="inline-flex items-center gap-2">
              <span>{data.environment}</span>
              {data.production ? (
                <Badge tone="ok" dot>
                  {t("config.production")}
                </Badge>
              ) : (
                <Badge tone="warn" dot>
                  {t("config.nonProduction")}
                </Badge>
              )}
            </span>
          ),
        },
        { k: t("config.keys"), v: <span className="tabular">{fmtNumber(locale, keys)}</span> },
        { k: t("config.maskedValue"), v: <span className="tabular">{fmtNumber(locale, masked)}</span> },
      ]}
    />
  );
}

function Section({ section, open, onToggle }: { section: ConfigSection; open: boolean; onToggle: () => void }) {
  const { t, locale } = useI18n();
  const maskedHere = section.leaves.filter((l) => l.masked).length;
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-surface-2 transition-colors"
      >
        {open ? <ChevronDown className="size-3.5 text-fg-faint" /> : <ChevronRight className="size-3.5 text-fg-faint" />}
        <Mono className="text-[13px] font-medium">{section.name}</Mono>
        <span className="text-xs text-fg-faint tabular">{fmtNumber(locale, section.leaves.length)}</span>
        {maskedHere ? (
          <Badge tone="neutral" className="ml-auto">
            {t("config.maskedCount", { n: fmtNumber(locale, maskedHere) })}
          </Badge>
        ) : null}
      </button>
      {open ? (
        <table className="w-full text-[13px]">
          <tbody>
            {section.leaves.map((leaf) => {
              const text = formatLeaf(leaf.value);
              return (
                <tr key={leaf.path} className="group border-t border-border/60">
                  <td className="pl-9 pr-3 py-1.5 align-top w-[40%]">
                    <Mono className="text-fg-muted break-all">{leaf.path}</Mono>
                  </td>
                  <td className="px-3 py-1.5 align-top">
                    <div className="flex items-start gap-2 min-w-0">
                      {leaf.masked ? (
                        <Badge tone="neutral">{t("config.maskedValue")}</Badge>
                      ) : (
                        <Mono className="break-all whitespace-pre-wrap">{text}</Mono>
                      )}
                      {!leaf.masked && text ? (
                        <CopyButton value={text} className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100 shrink-0" />
                      ) : null}
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      ) : null}
    </div>
  );
}
