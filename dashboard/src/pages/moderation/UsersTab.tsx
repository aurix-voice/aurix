import { Ban, Download, Gavel, MessageSquare, Plus, RefreshCw, ShieldAlert, Trash2, Users, X } from "lucide-react";
import { useState } from "react";

import { downloadJson, errorMessage, type T } from "@/api/client";
import {
  useAddUserBlockMutation,
  useApi,
  useDeleteUserMutation,
  useRemoveUserBlockMutation,
  useUnbanUserMutation,
  useUserBlocksQuery,
  useUserQuery,
  useUserRiskQuery,
  useUsersQuery,
} from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtMinutes, fmtRelative, shortId } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { usePageSize } from "@/lib/usePageSize";
import { Button } from "@/ui/Button";
import { ConfirmDialog, FormDialog } from "@/ui/Dialog";
import { Checkbox, Input } from "@/ui/Input";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Card, CardHeader, CodeBlock, EmptyState, Field, IdChip, KV, Mono, Skeleton } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { BanDialog } from "./BanDialog";
import { RiskBadge, UserRef, useModSearch } from "./shared";
import { isUuid, UserPicker } from "./UserPicker";

export function UsersTab() {
  const { t, locale } = useI18n();
  const now = useNow(30_000);
  const { user: selected, go } = useModSearch();
  const [q, setQ] = useState("");
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = usePageSize("users");
  const users = useUsersQuery({ q: q.trim() || undefined, page, per_page: perPage });
  const rows = users.data ?? [];

  const columns: Column<T.User>[] = [
    {
      key: "name",
      header: t("common.name"),
      sort: (u) => u.display_name,
      cell: (u) => (
        <span className="flex items-center gap-2">
          <span className="font-medium">{u.display_name || <span className="text-fg-faint">—</span>}</span>
          {u.is_banned ? (
            <Badge tone="danger" dot>
              {t("moderation.users.banned")}
            </Badge>
          ) : null}
        </span>
      ),
    },
    { key: "ext", header: t("moderation.users.externalId"), sort: (u) => u.external_id, cell: (u) => <Mono className="text-[12px]">{u.external_id}</Mono> },
    { key: "id", header: t("common.id"), width: "8rem", cell: (u) => <Mono className="text-[12px] text-fg-muted">{shortId(u.id)}</Mono> },
    {
      key: "seen",
      header: t("moderation.users.lastSeen"),
      width: "9rem",
      sort: (u) => u.last_seen_at ?? "",
      cell: (u) => (
        <span className="text-fg-muted" title={fmtDateTime(locale, u.last_seen_at)}>
          {u.last_seen_at ? fmtRelative(locale, u.last_seen_at, now) : t("common.never")}
        </span>
      ),
    },
    { key: "minutes", header: t("moderation.users.minutes"), width: "8rem", align: "right", sort: (u) => u.total_session_minutes ?? 0, cell: (u) => <span className="tabular-nums">{fmtMinutes(locale, u.total_session_minutes)}</span> },
    { key: "devices", header: t("moderation.users.devices"), width: "6rem", align: "right", cell: (u) => <span className="tabular-nums">{u.device_ids?.length ?? 0}</span> },
  ];

  return (
    <div className={selected ? "grid items-start gap-4 xl:grid-cols-[minmax(0,1fr)_440px]" : ""}>
      <div className="flex min-w-0 flex-col gap-3">
        <Toolbar
          end={
            <Button variant="ghost" size="icon" onClick={() => void users.refetch()} aria-label={t("common.refresh")} disabled={users.isFetching}>
              <RefreshCw className={users.isFetching ? "size-4 animate-spin" : "size-4"} />
            </Button>
          }
        >
          <Input
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setPage(1);
            }}
            placeholder={t("moderation.users.search")}
            className="w-96"
          />
        </Toolbar>
        <Card>
          <DataTable
            rows={rows}
            columns={columns}
            rowKey={(u) => u.id}
            loading={users.isPending}
            error={users.isError ? users.error : undefined}
            empty={<EmptyState compact icon={<Users className="size-5" />} title={q ? t("common.noResults") : t("moderation.users.empty")} />}
            onRowClick={(u) => go({ user: u.id === selected ? null : u.id })}
            selectedKey={selected}
            footer={
              <Pager
                page={page}
                hasPrev={page > 1}
                hasNext={rows.length >= perPage}
                onPage={(d) => setPage((p) => Math.max(1, p + d))}
                pageSize={perPage}
                onPageSize={(n) => {
                  setPerPage(n);
                  setPage(1);
                }}
                total={rows.length}
                totalLabel={t("common.count.items", { n: rows.length })}
              />
            }
          />
        </Card>
      </div>
      {selected ? <UserDetail key={selected} userId={selected} onClose={() => go({ user: null })} /> : null}
    </div>
  );
}

function UserDetail({ userId, onClose }: { userId: string; onClose: () => void }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const toast = useToast();
  const api = useApi();
  const now = useNow(30_000);
  const { go } = useModSearch();
  const detail = useUserQuery(userId);
  const risk = useUserRiskQuery(canApp("moderation:read") ? userId : null);
  const blocks = useUserBlocksQuery(userId);
  const unban = useUnbanUserMutation();
  const addBlock = useAddUserBlockMutation();
  const removeBlock = useRemoveUserBlockMutation();
  const del = useDeleteUserMutation();

  const [banning, setBanning] = useState(false);
  const [unbanning, setUnbanning] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [purge, setPurge] = useState(false);
  const [adding, setAdding] = useState(false);
  const [blockTarget, setBlockTarget] = useState("");
  const [removing, setRemoving] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
  const [showMeta, setShowMeta] = useState(false);

  const u = detail.data?.user;
  const sessions = detail.data?.active_sessions ?? [];

  const exportUser = async () => {
    setExporting(true);
    try {
      const data = await api.exportUser(userId);
      downloadJson(data, `user-${shortId(userId)}-export.json`);
    } catch (e) {
      toast.error(errorMessage(e, locale));
    } finally {
      setExporting(false);
    }
  };

  return (
    <Card className="flex flex-col gap-4 p-4">
      <CardHeader
        className="p-0"
        title={
          <span className="flex items-center gap-2">
            {u ? u.display_name || u.external_id : t("common.user")}
            {u?.is_banned ? (
              <Badge tone="danger" dot>
                {t("moderation.users.banned")}
              </Badge>
            ) : null}
            {risk.data && risk.data.risk_level !== "none" ? <RiskBadge risk={risk.data} /> : null}
          </span>
        }
        description={<IdChip id={userId} />}
        actions={
          <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
            <X className="size-4" />
          </Button>
        }
      />

      {detail.isError ? <QueryError error={detail.error} onRetry={() => void detail.refetch()} compact /> : null}
      {detail.isPending ? <Skeleton className="h-40" /> : null}

      {u ? (
        <>
          <KV
            cols={2}
            items={[
              { k: t("moderation.users.externalId"), v: <Mono className="break-all">{u.external_id}</Mono> },
              { k: t("common.created"), v: <span title={fmtDateTime(locale, u.created_at)}>{fmtRelative(locale, u.created_at, now)}</span> },
              { k: t("moderation.users.lastSeen"), v: u.last_seen_at ? <span title={fmtDateTime(locale, u.last_seen_at)}>{fmtRelative(locale, u.last_seen_at, now)}</span> : t("common.never") },
              { k: t("moderation.users.minutes"), v: fmtMinutes(locale, u.total_session_minutes) },
              { k: t("live.sessions"), v: sessions.length },
              {
                k: t("moderation.users.devices"),
                v: u.device_ids?.length ? (
                  <span className="flex flex-wrap gap-1">
                    {u.device_ids.map((d) => (
                      <Mono key={d} className="text-[11px]" title={d}>
                        {d.length > 14 ? `${d.slice(0, 12)}…` : d}
                      </Mono>
                    ))}
                  </span>
                ) : (
                  <span className="text-fg-faint">—</span>
                ),
              },
              { k: t("moderation.risk.incidents"), v: risk.data ? risk.data.incidents : <span className="text-fg-faint">—</span>, hidden: !risk.data },
            ]}
          />

          {u.is_banned ? (
            <div className="rounded-xl border border-danger/30 bg-danger-soft px-3 py-2 text-[12.5px]">
              <div className="font-medium">{t("moderation.users.banned")}</div>
              {u.ban_reason ? <div className="mt-0.5 text-fg-muted">{u.ban_reason}</div> : null}
              <div className="mt-0.5 text-fg-muted">
                {u.ban_expires_at ? `${t("common.expires")} ${fmtRelative(locale, u.ban_expires_at, now)}` : t("moderation.ban.permanent")}
              </div>
            </div>
          ) : null}

          {u.metadata && Object.keys(u.metadata).length ? (
            <div className="flex flex-col gap-1.5">
              <button type="button" className="self-start text-[12px] text-fg-muted hover:text-fg" onClick={() => setShowMeta((v) => !v)}>
                {showMeta ? t("common.hide") : t("common.show")} {t("moderation.users.metadata").toLowerCase()}
              </button>
              {showMeta ? <CodeBlock value={JSON.stringify(u.metadata, null, 2)} maxHeight="14rem" /> : null}
            </div>
          ) : null}

          <div className="flex flex-col gap-2">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-[11px] font-medium uppercase tracking-wide text-fg-faint">{t("moderation.users.blocks")}</div>
                <div className="text-[12px] text-fg-muted">{t("moderation.users.blocks.desc")}</div>
              </div>
              {canApp("users:write") ? (
                <Button size="xs" variant="outline" onClick={() => setAdding(true)}>
                  <Plus className="size-3.5" /> {t("moderation.users.addBlock")}
                </Button>
              ) : null}
            </div>
            {blocks.isError ? <QueryError error={blocks.error} onRetry={() => void blocks.refetch()} compact /> : null}
            {blocks.isPending ? <Skeleton className="h-8" /> : null}
            {blocks.data ? (
              blocks.data.blocked_users.length === 0 ? (
                <div className="text-[12.5px] text-fg-faint">{t("common.none")}</div>
              ) : (
                <ul className="flex flex-col divide-y divide-border rounded-xl border border-border">
                  {blocks.data.blocked_users.map((b) => (
                    <li key={b} className="flex items-center justify-between gap-2 px-3 py-1.5 text-[12.5px]">
                      <UserRef id={b} />
                      {canApp("users:write") ? (
                        <Button size="xs" variant="ghost" onClick={() => setRemoving(b)}>
                          {t("moderation.users.removeBlock")}
                        </Button>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )
            ) : null}
          </div>

          <div className="flex flex-wrap items-center gap-2 border-t border-border pt-3">
            {canApp("chat:read") ? (
              <Button size="sm" variant="outline" onClick={() => go({ tab: "chat", user: userId, channel: null }, false)}>
                <MessageSquare className="size-3.5" /> {t("moderation.users.messages")}
              </Button>
            ) : null}
            {canApp("moderation:read") ? (
              <Button size="sm" variant="outline" onClick={() => go({ tab: "events", user: userId }, false)}>
                <ShieldAlert className="size-3.5" /> {t("moderation.tabs.events")}
              </Button>
            ) : null}
            {canApp("moderation:write") ? (
              u.is_banned ? (
                <Button size="sm" variant="outline" onClick={() => setUnbanning(true)}>
                  <Ban className="size-3.5" /> {t("moderation.unban")}
                </Button>
              ) : (
                <Button size="sm" variant="outline" onClick={() => setBanning(true)}>
                  <Gavel className="size-3.5" /> {t("moderation.ban")}
                </Button>
              )
            ) : null}
            {canApp("users:export") ? (
              <Button size="sm" variant="ghost" loading={exporting} onClick={() => void exportUser()} title={t("moderation.users.export.desc")}>
                <Download className="size-3.5" /> {t("moderation.users.export")}
              </Button>
            ) : null}
            {canApp("users:write") ? (
              <Button size="sm" variant="ghost" className="ml-auto text-danger" onClick={() => setDeleting(true)}>
                <Trash2 className="size-3.5" /> {t("moderation.users.delete")}
              </Button>
            ) : null}
          </div>

          <BanDialog open={banning} onClose={() => setBanning(false)} userId={userId} who={u.display_name || u.external_id} />
          <ConfirmDialog
            open={unbanning}
            onOpenChange={(o) => !o && setUnbanning(false)}
            title={t("moderation.unban")}
            description={t("moderation.unban.desc")}
            confirmLabel={t("moderation.unban")}
            onConfirm={async () => {
              try {
                await unban.mutateAsync({ userId });
                toast.ok(t("moderation.unbanned"));
              } catch (e) {
                throw new Error(errorMessage(e, locale), { cause: e });
              }
            }}
          />
          <FormDialog
            open={adding}
            onOpenChange={(o) => {
              if (!o) {
                setBlockTarget("");
                setAdding(false);
              }
            }}
            title={t("moderation.users.addBlock")}
            description={t("moderation.users.blocks.desc")}
            submitLabel={t("moderation.users.addBlock")}
            size="sm"
            disabled={!isUuid(blockTarget) || blockTarget.trim() === userId}
            onSubmit={async () => {
              try {
                await addBlock.mutateAsync({ userId, blockedUserId: blockTarget.trim() });
                toast.ok(t("moderation.users.blockAdded"));
              } catch (e) {
                throw new Error(errorMessage(e, locale), { cause: e });
              }
            }}
          >
            <Field label={t("moderation.users.blockedUser")} required>
              <UserPicker value={blockTarget} onChange={setBlockTarget} autoFocus exclude={userId} />
            </Field>
          </FormDialog>
          <ConfirmDialog
            open={removing !== null}
            onOpenChange={(o) => !o && setRemoving(null)}
            title={t("moderation.users.removeBlock")}
            description={removing ? t("moderation.users.removeBlock.desc", { who: shortId(removing) }) : undefined}
            confirmLabel={t("moderation.users.removeBlock")}
            variant="danger"
            onConfirm={async () => {
              if (!removing) return;
              try {
                await removeBlock.mutateAsync({ userId, blockedUserId: removing });
                toast.ok(t("moderation.users.blockRemoved"));
              } catch (e) {
                throw new Error(errorMessage(e, locale), { cause: e });
              }
            }}
          />
          <ConfirmDialog
            open={deleting}
            onOpenChange={(o) => {
              if (!o) {
                setPurge(false);
                setDeleting(false);
              }
            }}
            title={t("moderation.users.delete")}
            description={t("moderation.users.delete.desc", { name: u.display_name || u.external_id })}
            confirmLabel={t("common.delete")}
            variant="danger"
            onConfirm={async () => {
              try {
                const r = await del.mutateAsync({ userId, purgeModeration: purge });
                toast.ok(t("moderation.users.deleted"), t("moderation.users.deleted.desc", { recordings: r.recordings_removed, sessions: r.rows_removed.sessions ?? 0, messages: r.rows_removed.chat_messages ?? 0 }));
                onClose();
              } catch (e) {
                throw new Error(errorMessage(e, locale), { cause: e });
              }
            }}
          >
            <Checkbox checked={purge} onChange={(e) => setPurge(e.target.checked)} label={t("moderation.users.delete.purge")} />
            <p className="text-[12px] text-fg-muted">{t("moderation.users.delete.purgeHint")}</p>
          </ConfirmDialog>
        </>
      ) : null}
    </Card>
  );
}
