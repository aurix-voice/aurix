import { Flag, RefreshCw, ShieldAlert } from "lucide-react";
import { useMemo, useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useModerationEventsQuery, useReportUserMutation, useSafetyIncidentsQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { FormDialog } from "@/ui/Dialog";
import { Input, NativeSelect, Textarea } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { Toolbar } from "@/ui/Page";
import { Card, EmptyState, Field, Mono } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { EventDetail } from "./EventDetail";
import { ChannelRef, type EVENT_STATUSES, EventTypeBadge, isSafetyType, StatusBadge, UserRef, useEventTypeLabel, useModSearch } from "./shared";
import { isUuid, UserPicker } from "./UserPicker";

const PER_PAGE = 50;
type StatusFilter = "" | (typeof EVENT_STATUSES)[number];

/**
 * Events (all moderation events, optional type filter client-side) and Incidents (`safety.*`
 * events with the server-side source/user filters) share the list + side panel.
 */
export function EventsTab({ mode }: { mode: "events" | "incidents" }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const now = useNow(15_000);
  const { event: selected, user: userFilterFromUrl, go } = useModSearch();
  const typeLabel = useEventTypeLabel();

  const [status, setStatus] = useState<StatusFilter>("pending");
  const [type, setType] = useState("");
  const [source, setSource] = useState<"" | T.SafetySource>("");
  const [userFilter, setUserFilter] = useState(userFilterFromUrl ?? "");
  const [page, setPage] = useState(1);
  const [reporting, setReporting] = useState(false);

  const userId = isUuid(userFilter) ? userFilter.trim() : undefined;
  const events = useModerationEventsQuery({ status: status || undefined, page, per_page: PER_PAGE }, mode === "events");
  const incidents = useSafetyIncidentsQuery({ status: status || undefined, source: source || undefined, user_id: userId, page, per_page: PER_PAGE }, mode === "incidents");
  const query = mode === "events" ? events : incidents;

  const rows = useMemo(() => {
    const list = query.data ?? [];
    if (mode === "incidents") return list;
    return list.filter((e) => (!type || e.event_type === type) && (!userId || e.target_user_id === userId || e.reporter_user_id === userId));
  }, [query.data, mode, type, userId]);

  const types = useMemo(() => {
    const set = new Set<string>();
    for (const e of events.data ?? []) set.add(e.event_type);
    for (const k of ["report", "kick", "mute", "ban", "safety.voice", "safety.text"]) set.add(k);
    return [...set].sort();
  }, [events.data]);

  const columns: Column<T.ModerationEvent>[] = [
    {
      key: "created",
      header: t("common.created"),
      width: "10rem",
      sort: (e) => e.created_at,
      cell: (e) => (
        <span className="text-fg-muted" title={fmtDateTime(locale, e.created_at)}>
          {fmtRelative(locale, e.created_at, now)}
        </span>
      ),
    },
    { key: "type", header: t("common.type"), width: "9rem", sort: (e) => e.event_type, cell: (e) => <EventTypeBadge type={e.event_type} /> },
    { key: "status", header: t("common.status"), width: "8rem", sort: (e) => e.status, cell: (e) => <StatusBadge status={e.status} /> },
    { key: "target", header: t("moderation.target"), width: "9rem", cell: (e) => <UserRef id={e.target_user_id} /> },
    { key: "channel", header: t("common.channel"), width: "9rem", cell: (e) => <ChannelRef id={e.channel_id} /> },
    {
      key: "reason",
      header: t("common.reason"),
      cell: (e) => (
        <span className="block max-w-[36rem] truncate" title={e.reason}>
          {e.reason || <span className="text-fg-faint">—</span>}
        </span>
      ),
    },
    {
      key: "by",
      header: mode === "incidents" ? t("moderation.incident.source") : t("moderation.reporter"),
      width: "8rem",
      hidden: mode === "incidents",
      cell: (e) => (e.moderator_user_id ? <UserRef id={e.moderator_user_id} /> : <UserRef id={e.reporter_user_id} />),
    },
  ];

  const empty = rows.length === 0 && (status || type || source || userFilter) ? t("common.noResults") : mode === "incidents" ? t("moderation.incidents.empty") : t("moderation.events.empty");

  return (
    <div className={selected ? "grid items-start gap-4 xl:grid-cols-[minmax(0,1fr)_440px]" : ""}>
      <div className="flex min-w-0 flex-col gap-3">
        <Toolbar
          end={
            <div className="flex items-center gap-2">
              <Button variant="ghost" size="icon" onClick={() => void query.refetch()} aria-label={t("common.refresh")} disabled={query.isFetching}>
                <RefreshCw className={query.isFetching ? "size-4 animate-spin" : "size-4"} />
              </Button>
              {mode === "events" && canApp("moderation:write") ? (
                <Button size="sm" variant="outline" onClick={() => setReporting(true)}>
                  <Flag className="size-3.5" /> {t("moderation.report")}
                </Button>
              ) : null}
            </div>
          }
        >
          <Segmented
            size="sm"
            value={status}
            onChange={(v) => {
              setStatus(v);
              setPage(1);
            }}
            options={[
              { value: "pending", label: t("moderation.status.pending") },
              { value: "resolved", label: t("moderation.status.resolved") },
              { value: "", label: t("common.all") },
            ]}
          />
          {mode === "events" ? (
            <NativeSelect value={type} onChange={(e) => setType(e.target.value)} className="h-8 w-44">
              <option value="">{t("moderation.filter.type")}: {t("common.all").toLowerCase()}</option>
              {types.map((k) => (
                <option key={k} value={k}>
                  {typeLabel(k)}
                </option>
              ))}
            </NativeSelect>
          ) : (
            <Segmented<"" | T.SafetySource>
              size="sm"
              value={source}
              onChange={(v) => {
                setSource(v);
                setPage(1);
              }}
              options={[
                { value: "", label: t("common.all") },
                { value: "voice", label: t("moderation.source.voice") },
                { value: "text", label: t("moderation.source.text") },
              ]}
            />
          )}
          <div className="w-72">
            <UserPicker
              value={userFilter}
              onChange={(v) => {
                setUserFilter(v);
                setPage(1);
              }}
              placeholder={t("moderation.filter.user")}
            />
          </div>
        </Toolbar>

        <Card>
          <DataTable
            rows={rows}
            columns={columns}
            rowKey={(e) => e.id}
            loading={query.isPending}
            error={query.isError ? errorMessage(query.error, locale) : undefined}
            empty={<EmptyState compact icon={<ShieldAlert className="size-5" />} title={empty} />}
            onRowClick={(e) => go({ event: e.id === selected ? null : e.id })}
            selectedKey={selected}
            rowClassName={(e) => (e.status === "pending" && isSafetyType(e.event_type) ? "bg-warn-soft/30" : undefined)}
            footer={
              <Pager
                page={page}
                hasPrev={page > 1}
                hasNext={(query.data?.length ?? 0) >= PER_PAGE}
                onPage={(d) => setPage((p) => Math.max(1, p + d))}
                total={rows.length}
                totalLabel={t("common.count.items", { n: rows.length })}
              />
            }
          />
        </Card>
      </div>
      {selected ? <EventDetail key={selected} eventId={selected} onClose={() => go({ event: null })} /> : null}
      <ReportDialog open={reporting} onClose={() => setReporting(false)} />
    </div>
  );
}

function ReportDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const report = useReportUserMutation();
  const [target, setTarget] = useState("");
  const [reporter, setReporter] = useState("");
  const [channel, setChannel] = useState("");
  const [reason, setReason] = useState("");
  const reset = () => {
    setTarget("");
    setReporter("");
    setChannel("");
    setReason("");
  };
  const ready = isUuid(target) && isUuid(reporter) && reason.trim() !== "" && (channel.trim() === "" || isUuid(channel));
  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        if (!o) {
          reset();
          onClose();
        }
      }}
      title={t("moderation.report")}
      description={t("moderation.report.desc")}
      submitLabel={t("moderation.report")}
      disabled={!ready}
      onSubmit={async () => {
        try {
          await report.mutateAsync({
            target_user_id: target.trim(),
            reporter_user_id: reporter.trim(),
            channel_id: channel.trim() || null,
            reason: reason.trim(),
          });
          toast.ok(t("moderation.report.done"));
        } catch (e) {
          throw new Error(errorMessage(e, locale), { cause: e });
        }
      }}
    >
      <Field label={t("moderation.target")} required>
        <UserPicker value={target} onChange={setTarget} autoFocus />
      </Field>
      <Field label={t("moderation.reporter")} required hint={t("moderation.report.reporterHint")}>
        <UserPicker value={reporter} onChange={setReporter} exclude={target.trim() || undefined} />
      </Field>
      <Field label={t("common.channel")} hint={t("common.optional")}>
        <Input value={channel} onChange={(e) => setChannel(e.target.value)} className="font-mono text-[12.5px]" spellCheck={false} placeholder={t("moderation.report.channelPlaceholder")} />
      </Field>
      <Field label={t("common.reason")} required>
        <Textarea value={reason} onChange={(e) => setReason(e.target.value)} rows={3} maxLength={2000} />
      </Field>
      {target && !isUuid(target) ? <Mono className="text-[11px] text-fg-faint">{t("moderation.userPicker.needUuid")}</Mono> : null}
    </FormDialog>
  );
}
