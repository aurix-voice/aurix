import { Disc3, Layers, Mic, RefreshCw } from "lucide-react";
import { useMemo, useState } from "react";

import { type T } from "@/api/client";
import { useChannelsQuery, useRecordingsQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtDateTime, fmtDuration, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Forbidden, RequireApp } from "@/shell/Guards";
import { Button } from "@/ui/Button";
import { Input, NativeSelect } from "@/ui/Input";
import { PageHeader, SplitLayout, Toolbar } from "@/ui/Page";
import { Callout, Card, EmptyState } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";

import { isUuid } from "../moderation/model";
import { ChannelRef, UserRef } from "../moderation/shared";
import { MixdownDialog, StartRecordingDialog } from "./Dialogs";
import { filterRecordings, isInFlight, mixdownCandidates, RECORDING_KINDS, RECORDING_STATUSES, selectionChannel, type RecordingFilter } from "./model";
import { RecordingDetail } from "./RecordingDetail";
import { RecordingKindBadge, RecordingStatusBadge, useRecordingsSearch } from "./shared";

const PER_PAGE = 50;

function Recordings() {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const now = useNow(30_000);
  const { id, channel, go } = useRecordingsSearch();
  const [page, setPage] = useState(1);
  const [channelInput, setChannelInput] = useState(channel ?? "");
  const [syncedChannel, setSyncedChannel] = useState(channel);
  if (channel !== syncedChannel) {
    setSyncedChannel(channel);
    setChannelInput(channel ?? "");
  }
  const [filter, setFilter] = useState<RecordingFilter>({ kind: "", status: "", user: "" });
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [startOpen, setStartOpen] = useState(false);
  const [mixOpen, setMixOpen] = useState(false);
  const canWrite = canApp("recordings:write");

  const channels = useChannelsQuery({ page: 1, per_page: 200, active_only: false }, false);
  const channelOptions = useMemo(() => (channels.data?.data ?? []).slice().sort((a, b) => a.name.localeCompare(b.name)), [channels.data]);
  const channelName = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of channels.data?.data ?? []) m.set(c.id, c.name);
    return (cid: string) => m.get(cid) ?? null;
  }, [channels.data]);

  const list = useRecordingsQuery({ channel_id: channel ?? undefined, page, per_page: PER_PAGE }, (d) => (d?.some(isInFlight) ? 5_000 : 30_000));
  const all = useMemo(() => list.data ?? [], [list.data]);
  const rows = useMemo(() => filterRecordings(all, filter), [all, filter]);

  const setChannel = (v: string) => {
    setChannelInput(v);
    setPage(1);
    setSelected(new Set());
    go({ channel: isUuid(v) ? v.trim() : null });
  };

  const mixChannel = channel ?? selectionChannel(all, selected);
  const mixReady = mixChannel !== null && mixdownCandidates(all, mixChannel).length > 0;
  const toggle = (rid: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(rid)) next.delete(rid);
      else next.add(rid);
      return next;
    });

  const columns: Column<T.Recording>[] = [
    {
      key: "sel",
      header: "",
      width: "2rem",
      hidden: !canWrite,
      cell: (r) =>
        r.kind === "recording" && r.status === "ready" ? (
          <input
            type="checkbox"
            className="size-3.5 accent-[var(--fg)] rounded"
            checked={selected.has(r.id)}
            onChange={() => toggle(r.id)}
            onClick={(e) => e.stopPropagation()}
            aria-label={t("recordings.selectForMixdown")}
          />
        ) : null,
    },
    { key: "kind", header: t("recordings.kind"), cell: (r) => <RecordingKindBadge kind={r.kind} /> },
    { key: "status", header: t("common.status"), cell: (r) => <RecordingStatusBadge status={r.status} /> },
    {
      key: "channel",
      header: t("common.channel"),
      hidden: channel !== null,
      cell: (r) => {
        const name = channelName(r.channel_id);
        return name ? (
          <span className="inline-flex items-center gap-1.5 min-w-0">
            <span className="truncate font-medium">{name}</span>
            <ChannelRef id={r.channel_id} />
          </span>
        ) : (
          <ChannelRef id={r.channel_id} />
        );
      },
    },
    {
      key: "user",
      header: t("common.user"),
      cell: (r) => (r.kind === "mixdown" ? <span className="text-fg-faint">{t("recordings.mixdown.tracks", { n: r.sources?.length ?? 0 })}</span> : <UserRef id={r.user_id} />),
    },
    { key: "started", header: t("recordings.started"), sort: (r) => r.started_at, cell: (r) => <span title={fmtDateTime(locale, r.started_at)}>{fmtRelative(locale, r.started_at, now)}</span> },
    { key: "duration", header: t("recordings.duration"), align: "right", sort: (r) => r.duration_secs ?? -1, cell: (r) => (r.duration_secs === undefined ? "—" : fmtDuration(locale, r.duration_secs)) },
    { key: "size", header: t("recordings.size"), align: "right", sort: (r) => r.file_size_bytes ?? -1, cell: (r) => (r.file_size_bytes === undefined ? "—" : <span className="whitespace-nowrap tabular">{fmtBytes(locale, r.file_size_bytes)}</span>) },
    { key: "format", header: t("recordings.format"), cell: (r) => <span className="text-fg-muted">{r.format === "wav" ? "WAV" : "Opus"}</span> },
    { key: "expires", header: t("recordings.expires"), sort: (r) => r.expires_at, cell: (r) => <span title={fmtDateTime(locale, r.expires_at)}>{fmtRelative(locale, r.expires_at, now)}</span> },
  ];

  const table = (
    <Card>
      <DataTable
        rows={rows}
        columns={columns}
        rowKey={(r) => r.id}
        loading={list.isPending}
        error={list.isError ? list.error : undefined}
        selectedKey={id}
        onRowClick={(r) => go({ id: r.id }, false)}
        empty={
          <EmptyState
            compact
            icon={<Disc3 className="size-5" />}
            title={all.length > 0 || channel || filter.kind || filter.status || filter.user ? t("common.noResults") : t("recordings.empty")}
            description={all.length === 0 && !channel ? t("recordings.empty.desc") : undefined}
          />
        }
        footer={
          <Pager
            page={page}
            hasPrev={page > 1}
            hasNext={all.length >= PER_PAGE}
            onPage={(d) => setPage((p) => Math.max(1, p + d))}
            total={rows.length}
            totalLabel={t("common.count.items", { n: rows.length })}
          />
        }
      />
    </Card>
  );

  return (
    <>
      <PageHeader
        title={t("recordings.title")}
        description={t("recordings.subtitle")}
        actions={
          canWrite ? (
            <div className="flex items-center gap-2">
              <Button variant="secondary" size="sm" disabled={!mixReady} title={!mixReady ? t("recordings.mixdown.pickChannel") : undefined} onClick={() => setMixOpen(true)}>
                <Layers className="size-3.5" />
                {t("recordings.mixdown")}
                {selected.size > 0 ? <span className="text-fg-faint tabular">{selected.size}</span> : null}
              </Button>
              <Button variant="primary" size="sm" onClick={() => setStartOpen(true)}>
                <Mic className="size-3.5" />
                {t("recordings.start")}
              </Button>
            </div>
          ) : undefined
        }
      />
      <div className="flex flex-col gap-3 pt-4">
        <Toolbar
          end={
            <Button variant="ghost" size="icon" onClick={() => void list.refetch()} aria-label={t("common.refresh")} disabled={list.isFetching}>
              <RefreshCw className={list.isFetching ? "size-4 animate-spin" : "size-4"} />
            </Button>
          }
        >
          <NativeSelect value={channelOptions.some((c) => c.id === channelInput) ? channelInput : ""} onChange={(e) => setChannel(e.target.value)} className="h-8 w-52">
            <option value="">{t("recordings.allChannels")}</option>
            {channelOptions.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </NativeSelect>
          <Input value={channelInput} onChange={(e) => setChannel(e.target.value)} placeholder={t("recordings.filter.channel")} className="h-8 w-72 font-mono text-[12.5px]" spellCheck={false} />
          <NativeSelect value={filter.kind} onChange={(e) => setFilter((f) => ({ ...f, kind: RECORDING_KINDS.find((k) => k === e.target.value) ?? "" }))} className="h-8 w-auto">
            <option value="">
              {t("recordings.filter.kind")}: {t("common.all")}
            </option>
            {RECORDING_KINDS.map((k) => (
              <option key={k} value={k}>
                {t(`recordings.kind.${k}`)}
              </option>
            ))}
          </NativeSelect>
          <NativeSelect value={filter.status} onChange={(e) => setFilter((f) => ({ ...f, status: RECORDING_STATUSES.find((k) => k === e.target.value) ?? "" }))} className="h-8 w-auto">
            <option value="">
              {t("recordings.filter.status")}: {t("common.all")}
            </option>
            {RECORDING_STATUSES.map((k) => (
              <option key={k} value={k}>
                {t(`recordings.status.${k}`)}
              </option>
            ))}
          </NativeSelect>
          <Input value={filter.user} onChange={(e) => setFilter((f) => ({ ...f, user: e.target.value }))} placeholder={t("recordings.filter.user")} className="h-8 w-56 font-mono text-[12.5px]" spellCheck={false} />
        </Toolbar>
        {channelInput.trim() !== "" && !isUuid(channelInput) ? <Callout tone="warn">{t("recordings.filter.channelInvalid")}</Callout> : null}
        {selected.size > 0 && mixChannel === null ? <Callout tone="warn">{t("recordings.mixdown.mixedChannels")}</Callout> : null}
        {id ? (
          <SplitLayout
            main={table}
            side={
              <RecordingDetail
                id={id}
                onClose={() => go({ id: null })}
                onDeleted={() => {
                  setSelected((prev) => {
                    const next = new Set(prev);
                    next.delete(id);
                    return next;
                  });
                  go({ id: null });
                }}
              />
            }
          />
        ) : (
          table
        )}
      </div>
      {startOpen ? <StartRecordingDialog open onOpenChange={setStartOpen} channelId={channel} onStarted={(r) => go({ id: r.id }, false)} /> : null}
      {mixOpen && mixChannel !== null ? (
        <MixdownDialog
          open
          onOpenChange={setMixOpen}
          recordings={all}
          channelId={mixChannel}
          initialSelection={selected}
          onQueued={(r) => {
            setSelected(new Set());
            go({ id: r.id }, false);
          }}
        />
      ) : null}
    </>
  );
}

export default function RecordingsPage() {
  const { canApp } = useAuth();
  return <RequireApp>{() => (canApp("recordings:read") ? <Recordings /> : <Forbidden perm="recordings:read" />)}</RequireApp>;
}
