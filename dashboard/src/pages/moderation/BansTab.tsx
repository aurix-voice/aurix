import { Gavel, RefreshCw } from "lucide-react";
import { useMemo, useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useBansQuery, useRevokeBanMutation } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { usePageSize } from "@/lib/usePageSize";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { Checkbox } from "@/ui/Input";
import { Toolbar } from "@/ui/Page";
import { Badge, Card, EmptyState, Mono } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { BanDialog } from "./BanDialog";
import { UserRef, useModSearch } from "./shared";
import { banState } from "./model";
import { isUuid, UserPicker } from "./UserPicker";

export function BanStateBadge({ ban, now }: { ban: T.Ban; now: number }) {
  const { t } = useI18n();
  const s = banState(ban, now);
  return (
    <Badge tone={s === "active" ? "danger" : "neutral"} dot={s === "active"}>
      {s === "active" ? t("common.active") : s === "expired" ? t("moderation.ban.expired") : t("moderation.ban.revoked")}
    </Badge>
  );
}

export function BanScopeBadge({ ban }: { ban: T.Ban }) {
  const { t } = useI18n();
  const label = ban.scope === "account" ? t("moderation.ban.user") : ban.scope === "device" ? t("moderation.ban.device") : t("moderation.ban.ip");
  const detail = ban.scope === "device" ? ban.device_id : ban.scope === "ip_address" ? ban.ip_address : null;
  return (
    <span className="inline-flex items-center gap-1.5">
      <Badge>{label}</Badge>
      {detail ? <Mono className="text-[11px] text-fg-muted">{detail}</Mono> : null}
    </span>
  );
}

export function BansTab() {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const now = useNow(30_000);
  const toast = useToast();
  const { user: userFromUrl } = useModSearch();
  const [userFilter, setUserFilter] = useState(userFromUrl ?? "");
  const [showInactive, setShowInactive] = useState(false);
  const [page, setPage] = useState(1);
  const [perPage, setPerPage] = usePageSize("bans");
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<T.Ban | null>(null);
  const revoke = useRevokeBanMutation();

  const userId = isUuid(userFilter) ? userFilter.trim() : undefined;
  const bans = useBansQuery({ user_id: userId, page, per_page: perPage });
  const rows = useMemo(() => (bans.data ?? []).filter((b) => showInactive || banState(b, now) === "active"), [bans.data, showInactive, now]);

  const columns: Column<T.Ban>[] = [
    { key: "state", header: t("common.status"), width: "7rem", cell: (b) => <BanStateBadge ban={b} now={now} /> },
    { key: "user", header: t("common.user"), width: "9rem", cell: (b) => <UserRef id={b.user_id} /> },
    { key: "scope", header: t("moderation.ban.scope"), width: "14rem", sort: (b) => b.scope, cell: (b) => <BanScopeBadge ban={b} /> },
    {
      key: "reason",
      header: t("common.reason"),
      cell: (b) => (
        <span className="block max-w-[28rem] truncate" title={b.reason}>
          {b.reason}
        </span>
      ),
    },
    {
      key: "created",
      header: t("common.created"),
      width: "9rem",
      sort: (b) => b.created_at,
      cell: (b) => (
        <span className="text-fg-muted" title={fmtDateTime(locale, b.created_at)}>
          {fmtRelative(locale, b.created_at, now)}
        </span>
      ),
    },
    {
      key: "expires",
      header: t("common.expires"),
      width: "9rem",
      sort: (b) => b.expires_at ?? "",
      cell: (b) =>
        b.expires_at ? (
          <span className="text-fg-muted" title={fmtDateTime(locale, b.expires_at)}>
            {fmtRelative(locale, b.expires_at, now)}
          </span>
        ) : (
          <span className="text-fg-muted">{t("moderation.ban.permanent")}</span>
        ),
    },
    { key: "by", header: t("moderation.ban.issuedBy"), width: "9rem", cell: (b) => <Mono title={b.issued_by}>{b.issued_by.length > 12 ? `${b.issued_by.slice(0, 8)}…` : b.issued_by}</Mono> },
    {
      key: "actions",
      header: "",
      width: "6rem",
      align: "right",
      hidden: !canApp("moderation:write"),
      cell: (b) =>
        banState(b, now) === "active" ? (
          <Button
            size="xs"
            variant="ghost"
            onClick={(e) => {
              e.stopPropagation();
              setRevoking(b);
            }}
          >
            {t("moderation.ban.revoke")}
          </Button>
        ) : null,
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <Toolbar
        end={
          <div className="flex items-center gap-2">
            <Button variant="ghost" size="icon" onClick={() => void bans.refetch()} aria-label={t("common.refresh")} disabled={bans.isFetching}>
              <RefreshCw className={bans.isFetching ? "size-4 animate-spin" : "size-4"} />
            </Button>
            {canApp("moderation:write") ? (
              <Button size="sm" onClick={() => setCreating(true)}>
                <Gavel className="size-3.5" /> {t("moderation.ban.new")}
              </Button>
            ) : null}
          </div>
        }
      >
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
        <Checkbox checked={showInactive} onChange={(e) => setShowInactive(e.target.checked)} label={t("moderation.ban.showRevoked")} />
      </Toolbar>

      <Card>
        <DataTable
          rows={rows}
          columns={columns}
          rowKey={(b) => b.id}
          loading={bans.isPending}
          error={bans.isError ? bans.error : undefined}
          empty={<EmptyState compact icon={<Gavel className="size-5" />} title={userFilter || !showInactive ? t("common.noResults") : t("moderation.bans.empty")} />}
          footer={
            <Pager
              page={page}
              hasPrev={page > 1}
              hasNext={(bans.data?.length ?? 0) >= perPage}
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

      <BanDialog open={creating} onClose={() => setCreating(false)} />
      <ConfirmDialog
        open={revoking !== null}
        onOpenChange={(o) => !o && setRevoking(null)}
        title={t("moderation.ban.revoke")}
        description={t("moderation.ban.revoke.desc")}
        confirmLabel={t("moderation.ban.revoke")}
        variant="danger"
        onConfirm={async () => {
          if (!revoking) return;
          try {
            await revoke.mutateAsync({ banId: revoking.id });
            toast.ok(t("moderation.ban.revokedDone"));
          } catch (e) {
            throw new Error(errorMessage(e, locale), { cause: e });
          }
        }}
      />
    </div>
  );
}
