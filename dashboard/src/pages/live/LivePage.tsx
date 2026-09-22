import { useNavigate } from "@tanstack/react-router";
import { Plus, Radio, RefreshCw, Search, Users } from "lucide-react";
import { useMemo, useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useEvents } from "@/api/events";
import { useChannelsQuery, useCreateChannelMutation, useEventSnapshotQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { EventFeed } from "@/components/EventFeed";
import { useI18n } from "@/i18n";
import { fmtNumber, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { usePageSize } from "@/lib/usePageSize";
import { RequireApp } from "@/shell/Guards";
import { Button } from "@/ui/Button";
import { FormDialog } from "@/ui/Dialog";
import { Input, NativeSelect } from "@/ui/Input";
import { Menu, Segmented } from "@/ui/Menu";
import { PageHeader, QueryError, SplitLayout, Toolbar } from "@/ui/Page";
import { Badge, Card, EmptyState, Field, IdChip, Stat } from "@/ui/Primitives";
import { DataTable, Pager, type Column, type SortState } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { ChannelFlags, ChannelTypeBadge } from "./ChannelBadges";
import { ChannelActionDialogs, useChannelMenuItems, type ChannelAction } from "./ChannelActions";
import { CHANNEL_TYPES, ChannelConfigForm, configProblems, parseConfigJson, type ConfigMode } from "./ChannelConfigForm";

type Scope = "active" | "all";

export default function LivePage() {
  return <RequireApp>{() => <LiveChannels />}</RequireApp>;
}

function LiveChannels() {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const navigate = useNavigate();
  const now = useNow(15_000);
  const { state: sse } = useEvents();

  const [scope, setScope] = useState<Scope>("active");
  const [type, setType] = useState<T.ChannelType | "">("");
  const [q, setQ] = useState("");
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = usePageSize("live-channels");
  const [sort, setSort] = useState<SortState | null>({ key: "participants", dir: "desc" });
  const [creating, setCreating] = useState(false);
  const [target, setTarget] = useState<{ channel: T.Channel; action: ChannelAction } | null>(null);

  const channels = useChannelsQuery({ page, per_page: perPage, active_only: scope === "active" });
  const snapshot = useEventSnapshotQuery();

  const rows = useMemo(() => {
    const list = (channels.data?.data ?? []).filter((c) => !type || c.channel_type === type);
    const needle = q.trim().toLowerCase();
    return needle ? list.filter((c) => c.name.toLowerCase().includes(needle) || c.id.startsWith(needle)) : list;
  }, [channels.data, q, type]);

  const totals = useMemo(() => {
    const snap = snapshot.data?.channels ?? [];
    const participants = snap.reduce((n, c) => n + (c.participants?.length ?? 0), 0);
    const muted = snap.reduce((n, c) => n + (c.participants?.filter((p) => p.is_muted || p.is_server_muted).length ?? 0), 0);
    return { activeChannels: snap.length, participants, muted };
  }, [snapshot.data]);

  if (!canApp("channels:read")) {
    return <EmptyState title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "channels:read" })} />;
  }

  const columns: Column<T.Channel>[] = [
    {
      key: "name",
      header: t("common.name"),
      sort: (c) => c.name,
      cell: (c) => (
        <div className="flex flex-col gap-0.5 min-w-0">
          <span className="font-medium truncate">{c.name}</span>
          <IdChip id={c.id} />
        </div>
      ),
    },
    { key: "type", header: t("live.channelType"), sort: (c) => c.channel_type, cell: (c) => <ChannelTypeBadge type={c.channel_type} /> },
    {
      key: "participants",
      header: t("live.participants"),
      align: "right",
      sort: (c) => c.active_participants ?? 0,
      cell: (c) => {
        const n = c.active_participants ?? 0;
        return (
          <span className={n > 0 ? "tabular font-medium" : "tabular text-fg-faint"}>
            {fmtNumber(locale, n)}
            <span className="text-fg-faint"> / {fmtNumber(locale, c.max_participants)}</span>
          </span>
        );
      },
    },
    { key: "flags", header: t("live.flags"), cell: (c) => <ChannelFlags config={c.config} adHoc={c.ad_hoc} persistent={c.is_persistent} /> },
    {
      key: "bitrate",
      header: t("live.bitrate"),
      align: "right",
      sort: (c) => c.config.bitrate ?? 0,
      cell: (c) => <span className="tabular text-fg-muted">{c.config.bitrate ? `${Math.round(c.config.bitrate / 1000)} kbps` : "—"}</span>,
    },
    {
      key: "created",
      header: t("common.created"),
      sort: (c) => c.created_at,
      cell: (c) => <span className="text-fg-muted">{fmtRelative(locale, c.created_at, now)}</span>,
    },
    {
      key: "actions",
      header: "",
      align: "right",
      width: "3rem",
      cell: (c) => (
        <span onClick={(e) => e.stopPropagation()}>
          <RowMenu channel={c} onAction={(action) => setTarget({ channel: c, action })} />
        </span>
      ),
    },
  ];

  const total = channels.data?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / perPage));

  return (
    <>
      <PageHeader
        title={t("live.title")}
        description={t("live.subtitle")}
        actions={
          <>
            <Button variant="ghost" size="sm" onClick={() => void Promise.all([channels.refetch(), snapshot.refetch()])} loading={channels.isFetching && !channels.isPending}>
              <RefreshCw className="size-3.5" />
              {t("common.refresh")}
            </Button>
            {canApp("channels:write") ? (
              <Button variant="primary" size="sm" onClick={() => setCreating(true)} data-testid="new-channel">
                <Plus className="size-3.5" />
                {t("live.newChannel")}
              </Button>
            ) : null}
          </>
        }
      />

      <div className="grid gap-3 sm:grid-cols-3 mb-4">
        <Stat label={t("live.activeChannels")} value={snapshot.isPending ? "…" : fmtNumber(locale, totals.activeChannels)} hint={t("live.activeChannels.hint")} />
        <Stat label={t("live.participants")} value={snapshot.isPending ? "…" : fmtNumber(locale, totals.participants)} hint={t("live.mutedNow", { n: totals.muted })} />
        <Stat
          label={t("live.eventStream")}
          value={
            <span className="inline-flex items-center gap-2">
              <Radio className={sse === "live" ? "size-4 text-ok" : "size-4 text-fg-faint"} />
              {t(sse === "live" ? "live.stream.live" : sse === "connecting" ? "live.stream.connecting" : "live.stream.offline")}
            </span>
          }
          hint={t("live.eventStream.hint")}
        />
      </div>

      <SplitLayout
        main={
          <Card>
            <Toolbar
              end={
                <Segmented<Scope>
                  size="sm"
                  value={scope}
                  onChange={(v) => {
                    setScope(v);
                    setPage(1);
                  }}
                  options={[
                    { value: "active", label: t("live.scope.active") },
                    { value: "all", label: t("live.scope.all") },
                  ]}
                />
              }
            >
              <div className="relative">
                <Search className="size-3.5 absolute left-2.5 top-1/2 -translate-y-1/2 text-fg-faint" />
                <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder={t("live.search")} className="pl-8 w-64" data-testid="channel-search" />
              </div>
              <NativeSelect value={type} onChange={(e) => setType(e.target.value as T.ChannelType | "")} className="w-40" aria-label={t("live.channelType")}>
                <option value="">{t("live.allTypes")}</option>
                {CHANNEL_TYPES.map((ct) => (
                  <option key={ct} value={ct}>
                    {ct}
                  </option>
                ))}
              </NativeSelect>
              {q || type ? <Badge tone="neutral">{t("live.filtered", { n: rows.length })}</Badge> : null}
            </Toolbar>
            <DataTable
              rows={rows}
              columns={columns}
              rowKey={(c) => c.id}
              loading={channels.isPending}
              error={channels.isError ? <QueryError error={channels.error} onRetry={() => void channels.refetch()} compact /> : undefined}
              empty={
                <EmptyState
                  compact
                  icon={<Users className="size-5" />}
                  title={scope === "active" ? t("live.empty.active") : t("live.empty.all")}
                  description={scope === "active" ? t("live.empty.active.desc") : t("live.empty.all.desc")}
                  action={
                    scope === "active" && total === 0 && !q ? (
                      <Button variant="secondary" size="sm" onClick={() => setScope("all")}>
                        {t("live.scope.all")}
                      </Button>
                    ) : undefined
                  }
                />
              }
              sort={sort}
              onSortChange={setSort}
              onRowClick={(c) => void navigate({ to: "/live/$channelId", params: { channelId: c.id }, search: {} })}
              footer={
                <Pager
                  page={page}
                  pages={pages}
                  hasPrev={page > 1}
                  hasNext={page < pages}
                  onPage={(d) => setPage((p) => p + d)}
                  pageSize={perPage}
                  onPageSize={(n) => {
                    setPerPage(n);
                    setPage(1);
                  }}
                  total={total}
                />
              }
            />
          </Card>
        }
        side={<EventFeed height={560} />}
      />

      <CreateChannelDialog open={creating} onOpenChange={setCreating} onCreated={(c) => void navigate({ to: "/live/$channelId", params: { channelId: c.id }, search: {} })} />
      <ChannelActionDialogs channel={target?.channel ?? null} action={target?.action ?? null} onClose={() => setTarget(null)} />
    </>
  );
}

function RowMenu({ channel, onAction }: { channel: T.Channel; onAction: (a: ChannelAction) => void }) {
  const items = useChannelMenuItems(channel, onAction);
  return <Menu items={items} />;
}

const DEFAULT_CONFIG: T.ChannelConfig = { channel_type: "team", max_participants: 64, bitrate: 48000, enable_dtx: true, enable_fec: true };

export function CreateChannelDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange: (o: boolean) => void; onCreated: (c: T.Channel) => void }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const create = useCreateChannelMutation();
  const [name, setName] = useState("");
  const [config, setConfig] = useState<T.ChannelConfig>(DEFAULT_CONFIG);
  const [mode, setMode] = useState<ConfigMode>("form");
  const [json, setJson] = useState(() => JSON.stringify(DEFAULT_CONFIG, null, 2));
  const [jsonError, setJsonError] = useState<string | null>(null);

  const switchMode = (m: ConfigMode) => {
    if (m === "json") setJson(JSON.stringify(config, null, 2));
    else {
      const r = parseConfigJson(json);
      if ("error" in r) {
        setJsonError(r.error);
        return;
      }
      setConfig(r.config);
    }
    setJsonError(null);
    setMode(m);
  };

  const reset = () => {
    setName("");
    setConfig(DEFAULT_CONFIG);
    setJson(JSON.stringify(DEFAULT_CONFIG, null, 2));
    setJsonError(null);
    setMode("form");
  };

  const submit = async () => {
    let cfg = config;
    if (mode === "json") {
      const r = parseConfigJson(json);
      if ("error" in r) {
        setJsonError(r.error);
        throw new Error(t("live.config.jsonInvalid"));
      }
      cfg = r.config;
    }
    const problems = configProblems(cfg);
    if (problems.length) throw new Error(t("live.config.invalid", { fields: problems.join(", ") }));
    try {
      const created = await create.mutateAsync({ name: name.trim(), config: cfg });
      toast.ok(t("live.channelCreated"));
      reset();
      onCreated(created);
    } catch (e) {
      throw new Error(errorMessage(e, locale), { cause: e });
    }
  };

  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        if (!o) reset();
        onOpenChange(o);
      }}
      title={t("live.newChannel")}
      description={t("live.newChannel.desc")}
      submitLabel={t("common.create")}
      size="lg"
      disabled={!name.trim()}
      onSubmit={submit}
    >
      <Field label={t("common.name")} required>
        <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={128} autoFocus data-testid="channel-name" />
      </Field>
      <ChannelConfigForm value={config} onChange={setConfig} mode={mode} onModeChange={switchMode} json={json} onJsonChange={setJson} jsonError={jsonError} />
    </FormDialog>
  );
}
