import { useNavigate } from "@tanstack/react-router";
import { Boxes, Plus } from "lucide-react";
import { useMemo, useState } from "react";

import type { T } from "@/api/client";
import { useAppsQuery, useCreateAppMutation } from "@/api/hooks";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDate, fmtDateTime, fmtNumber } from "@/lib/format";
import { PAGE_SIZES, usePageSize } from "@/lib/usePageSize";
import { Button } from "@/ui/Button";
import { FormDialog } from "@/ui/Dialog";
import { Input } from "@/ui/Input";
import { PageHeader, QueryError, Toolbar } from "@/ui/Page";
import { Badge, Card, EmptyState, IdChip } from "@/ui/Primitives";
import { DataTable, Pager, sortRows, type Column, type SortState } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { AppFormFields, EMPTY_APP_FORM, parseLimit, type AppFormValues } from "./AppForm";
import { SecretReveal } from "./SecretReveal";

function limitText(locale: "en" | "ru", v: number | undefined, unlimitedLabel: string): string {
  if (v === undefined || v === null) return "—";
  return v === 0 ? unlimitedLabel : fmtNumber(locale, v);
}

export default function AppsPage() {
  const { t, locale } = useI18n();
  const { can } = useAuth();
  const { appId: scopeId, setAppId } = useAppScope();
  const navigate = useNavigate();
  const toast = useToast();
  const apps = useAppsQuery();
  const create = useCreateAppMutation();

  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<SortState | null>({ key: "created", dir: "desc" });
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = usePageSize("apps");
  const [creating, setCreating] = useState(false);
  const [form, setForm] = useState<AppFormValues>(EMPTY_APP_FORM);
  const [created, setCreated] = useState<T.CreatedApp | null>(null);

  const columns = useMemo<Column<T.App>[]>(
    () => [
      {
        key: "name",
        header: t("common.name"),
        sort: (a) => a.name,
        cell: (a) => (
          <div className="flex flex-col min-w-0 max-w-[18rem]">
            <span className="font-medium truncate" title={a.name}>{a.name}</span>
            {a.description ? <span className="text-xs text-fg-muted truncate max-w-[16rem]">{a.description}</span> : null}
          </div>
        ),
      },
      { key: "id", header: t("common.id"), cell: (a) => <IdChip id={a.id} /> },
      {
        key: "status",
        header: t("common.status"),
        sort: (a) => (a.active ? 1 : 0),
        cell: (a) => (
          <span className="inline-flex items-center gap-1.5">
            <Badge tone={a.active ? "ok" : "neutral"} dot>
              {a.active ? t("common.active") : t("common.inactive")}
            </Badge>
            {a.id === scopeId ? <Badge tone="accent">{t("apps.inScope")}</Badge> : null}
          </span>
        ),
      },
      {
        key: "ccu",
        header: t("apps.col.ccu"),
        align: "right",
        sort: (a) => a.max_concurrent_sessions ?? 0,
        cell: (a) => <span className="tabular">{limitText(locale, a.max_concurrent_sessions, t("common.unlimited"))}</span>,
      },
      {
        key: "minutes",
        header: t("apps.col.minutes"),
        align: "right",
        sort: (a) => a.monthly_participant_minutes ?? 0,
        cell: (a) => <span className="tabular">{limitText(locale, a.monthly_participant_minutes, t("common.unlimited"))}</span>,
      },
      {
        key: "channels",
        header: t("apps.col.channels"),
        align: "right",
        sort: (a) => a.max_channels ?? 0,
        cell: (a) => <span className="tabular">{limitText(locale, a.max_channels, t("common.unlimited"))}</span>,
      },
      {
        key: "created",
        header: t("common.created"),
        sort: (a) => Date.parse(a.created_at),
        cell: (a) => <span className="text-fg-muted whitespace-nowrap" title={fmtDateTime(locale, a.created_at)}>{fmtDate(locale, a.created_at)}</span>,
      },
      {
        key: "scope",
        header: "",
        align: "right",
        cell: (a) =>
          a.id === scopeId ? null : (
            <Button
              size="xs"
              variant="ghost"
              onClick={(e) => {
                e.stopPropagation();
                setAppId(a.id);
              }}
            >
              {t("apps.useAsScope")}
            </Button>
          ),
      },
    ],
    [t, locale, scopeId, setAppId],
  );

  const rows = useMemo(() => {
    const q = search.trim().toLowerCase();
    const list = (apps.data ?? []).filter((a) => !q || a.name.toLowerCase().includes(q) || a.id.startsWith(q) || (a.description ?? "").toLowerCase().includes(q));
    return sortRows(list, columns, sort);
  }, [apps.data, search, columns, sort]);
  const pages = Math.max(1, Math.ceil(rows.length / perPage));
  const current = Math.min(page, pages);
  const pageRows = useMemo(() => rows.slice((current - 1) * perPage, current * perPage), [rows, current, perPage]);

  const submit = async () => {
    const name = form.name.trim();
    if (!name) throw new Error(t("common.required"));
    const limits = {
      max_channels: parseLimit(form.max_channels, 1),
      max_participants_per_channel: parseLimit(form.max_participants_per_channel, 1),
      max_concurrent_sessions: parseLimit(form.max_concurrent_sessions),
      monthly_participant_minutes: parseLimit(form.monthly_participant_minutes),
    };
    if (Object.values(limits).some((v) => v === null)) throw new Error(t("apps.limits.invalid"));
    const res = await create.mutateAsync({
      name,
      description: form.description.trim() || null,
      max_channels: limits.max_channels ?? null,
      max_participants_per_channel: limits.max_participants_per_channel ?? null,
      max_concurrent_sessions: limits.max_concurrent_sessions ?? null,
      monthly_participant_minutes: limits.monthly_participant_minutes ?? null,
    });
    setForm(EMPTY_APP_FORM);
    setCreated(res);
    toast.ok(t("apps.created"));
  };

  if (!can("apps:read")) {
    return <EmptyState title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "apps:read" })} />;
  }

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title={t("apps.title")}
        description={t("apps.subtitle")}
        actions={
          can("apps:write") ? (
            <Button variant="primary" size="sm" onClick={() => setCreating(true)}>
              <Plus className="size-4" />
              {t("apps.new")}
            </Button>
          ) : null
        }
      />

      <Card>
        <Toolbar end={<span className="text-xs text-fg-muted tabular">{t("common.count.items", { n: rows.length })}</span>}>
          <Input
            value={search}
            onChange={(e) => {
              setSearch(e.target.value);
              setPage(1);
            }}
            placeholder={t("common.search")}
            className="w-64"
            aria-label={t("common.search")}
          />
        </Toolbar>
        <DataTable
          rows={pageRows}
          columns={columns}
          rowKey={(a) => a.id}
          loading={apps.isPending}
          error={apps.isError ? <QueryError error={apps.error} onRetry={() => void apps.refetch()} compact /> : undefined}
          sort={sort}
          onSortChange={(s) => {
            setSort(s);
            setPage(1);
          }}
          selectedKey={scopeId}
          onRowClick={(a) => void navigate({ to: "/apps/$appId", params: { appId: a.id }, search: {} })}
          footer={
            rows.length > PAGE_SIZES[0] ? (
              <Pager
                page={current}
                pages={pages}
                hasPrev={current > 1}
                hasNext={current < pages}
                onPage={(d) => setPage(current + d)}
                pageSize={perPage}
                onPageSize={(n) => {
                  setPerPage(n);
                  setPage(1);
                }}
                total={rows.length}
              />
            ) : undefined
          }
          empty={
            <EmptyState
              compact
              icon={<Boxes className="size-6" strokeWidth={1.5} />}
              title={search ? t("common.noResults") : t("apps.empty")}
              description={search ? undefined : t("apps.empty.desc")}
              action={
                !search && can("apps:write") ? (
                  <Button variant="primary" size="sm" onClick={() => setCreating(true)}>
                    <Plus className="size-4" />
                    {t("apps.new")}
                  </Button>
                ) : undefined
              }
            />
          }
        />
      </Card>

      <FormDialog open={creating} onOpenChange={setCreating} title={t("apps.new")} submitLabel={t("common.create")} onSubmit={submit}>
        <AppFormFields value={form} onChange={setForm} autoFocus />
      </FormDialog>

      <SecretReveal
        secret={created?.api_key ?? null}
        title={t("apps.created")}
        notice={t("apps.created.keyNotice")}
        meta={
          created
            ? [
                { k: t("common.name"), v: created.name },
                { k: t("common.id"), v: <IdChip id={created.id} short={36} /> },
                { k: t("keys.default"), v: <IdChip id={created.api_key_id} short={36} /> },
              ]
            : undefined
        }
        onClose={() => {
          const id = created?.id;
          setCreated(null);
          if (id) void navigate({ to: "/apps/$appId", params: { appId: id }, search: {} });
        }}
      />
    </div>
  );
}
