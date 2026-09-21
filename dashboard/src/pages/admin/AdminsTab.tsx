import { KeyRound, LogOut, MoreHorizontal, Plus, ShieldCheck, UserCheck, UserX, Users } from "lucide-react";
import { useCallback, useMemo, useState } from "react";

import type { T } from "@/api/client";
import {
  useAdminsQuery,
  useAuthMethodsQuery,
  useCreateAdminMutation,
  useResetAdminPasswordMutation,
  useRevokeAdminTokensMutation,
  useUpdateAdminMutation,
} from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { ConfirmDialog, FormDialog } from "@/ui/Dialog";
import { Checkbox, Input } from "@/ui/Input";
import { Menu } from "@/ui/Menu";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, Card, EmptyState, Field, IdChip, Mono } from "@/ui/Primitives";
import { DataTable, type Column, type SortState } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { filterAdmins, otherActiveSuperadmins, passwordValid } from "./model";
import { RoleBadge, RoleField, SourceBadge } from "./shared";

interface CreateForm {
  email: string;
  display_name: string;
  role: T.AdminRole;
  password: string;
}

const EMPTY_CREATE: CreateForm = { email: "", display_name: "", role: "admin", password: "" };

type Action = { kind: "edit"; admin: T.Admin } | { kind: "deactivate"; admin: T.Admin } | { kind: "activate"; admin: T.Admin } | { kind: "reset"; admin: T.Admin } | { kind: "revoke"; admin: T.Admin };

export function AdminsTab() {
  const { t, locale } = useI18n();
  const { admin: me } = useAuth();
  const toast = useToast();
  const now = useNow();
  const admins = useAdminsQuery();
  const methods = useAuthMethodsQuery();
  const create = useCreateAdminMutation();
  const update = useUpdateAdminMutation();
  const reset = useResetAdminPasswordMutation();
  const revoke = useRevokeAdminTokensMutation();

  const [search, setSearch] = useState("");
  const [showInactive, setShowInactive] = useState(false);
  const [sort, setSort] = useState<SortState | null>({ key: "role", dir: "desc" });
  const [creating, setCreating] = useState(false);
  const [form, setForm] = useState<CreateForm>(EMPTY_CREATE);
  const [action, setAction] = useState<Action | null>(null);
  const [editRole, setEditRole] = useState<T.AdminRole>("admin");
  const [editName, setEditName] = useState("");
  const [newPassword, setNewPassword] = useState("");

  const list = useMemo(() => admins.data ?? [], [admins.data]);
  const inactive = list.filter((a) => !a.active).length;

  const openAction = useCallback((a: Action) => {
    if (a.kind === "edit") {
      setEditRole(a.admin.role);
      setEditName(a.admin.display_name);
    }
    if (a.kind === "reset") setNewPassword("");
    setAction(a);
  }, []);

  const lastSuperadmin = useCallback((a: T.Admin) => a.active && a.role === "superadmin" && otherActiveSuperadmins(list, a.id) === 0, [list]);

  const columns = useMemo<Column<T.Admin>[]>(
    () => [
      {
        key: "name",
        header: t("admin.displayName"),
        sort: (a) => a.display_name,
        cell: (a) => (
          <div className="flex flex-col min-w-0">
            <span className="font-medium truncate inline-flex items-center gap-1.5">
              {a.display_name}
              {a.id === me?.id ? <Badge tone="accent">{t("admin.you")}</Badge> : null}
            </span>
            <span className="text-xs text-fg-muted truncate">{a.email}</span>
          </div>
        ),
      },
      { key: "id", header: t("common.id"), cell: (a) => <IdChip id={a.id} /> },
      { key: "role", header: t("admin.role"), sort: (a) => ["viewer", "moderator", "admin", "superadmin"].indexOf(a.role), cell: (a) => <RoleBadge role={a.role} /> },
      {
        key: "status",
        header: t("common.status"),
        sort: (a) => (a.active ? 1 : 0),
        cell: (a) => (
          <Badge tone={a.active ? "ok" : "neutral"} dot>
            {a.active ? t("common.active") : t("common.inactive")}
          </Badge>
        ),
      },
      { key: "source", header: t("auth.source"), cell: (a) => <SourceBadge admin={a} /> },
      {
        key: "lastLogin",
        header: t("admin.lastLogin"),
        sort: (a) => (a.last_login_at ? Date.parse(a.last_login_at) : null),
        cell: (a) =>
          a.last_login_at ? (
            <span className="text-fg-muted whitespace-nowrap" title={fmtDateTime(locale, a.last_login_at)}>
              {fmtRelative(locale, a.last_login_at, now)}
            </span>
          ) : (
            <span className="text-fg-faint">{t("common.never")}</span>
          ),
      },
      {
        key: "created",
        header: t("common.created"),
        sort: (a) => (a.created_at ? Date.parse(a.created_at) : null),
        cell: (a) => <span className="text-fg-muted whitespace-nowrap">{fmtDateTime(locale, a.created_at)}</span>,
      },
      {
        key: "actions",
        header: "",
        align: "right",
        cell: (a) => {
          const self = a.id === me?.id;
          const last = lastSuperadmin(a);
          return (
            <Menu
              label={t("common.actions")}
              trigger={
                <Button size="icon" variant="ghost" aria-label={t("common.actions")}>
                  <MoreHorizontal className="size-4" />
                </Button>
              }
              items={[
                { label: t("admin.edit"), icon: <ShieldCheck className="size-4" />, onSelect: () => openAction({ kind: "edit", admin: a }) },
                {
                  label: t("admin.resetPassword"),
                  icon: <KeyRound className="size-4" />,
                  onSelect: () => openAction({ kind: "reset", admin: a }),
                  hidden: self,
                },
                { label: t("admin.revokeTokens"), icon: <LogOut className="size-4" />, onSelect: () => openAction({ kind: "revoke", admin: a }), hidden: self },
                {
                  label: a.active ? t("admin.deactivate") : t("admin.activate"),
                  icon: a.active ? <UserX className="size-4" /> : <UserCheck className="size-4" />,
                  danger: a.active,
                  separatorBefore: true,
                  disabled: self || last,
                  onSelect: () => openAction({ kind: a.active ? "deactivate" : "activate", admin: a }),
                },
              ]}
            />
          );
        },
      },
    ],
    [t, locale, now, me?.id, openAction, lastSuperadmin],
  );

  const rows = useMemo(() => filterAdmins(list, { search, showInactive }), [list, search, showInactive]);

  const submitCreate = async () => {
    const email = form.email.trim();
    const display_name = form.display_name.trim();
    if (!email || !display_name) throw new Error(t("common.required"));
    if (!passwordValid(form.password)) throw new Error(t("admin.password.invalid"));
    await create.mutateAsync({ email, display_name, role: form.role, password: form.password });
    toast.ok(t("admin.created"));
    setCreating(false);
    setForm(EMPTY_CREATE);
  };

  const submitEdit = async () => {
    if (action?.kind !== "edit") return;
    const body: T.UpdateAdminRequest = {};
    const name = editName.trim();
    if (!name) throw new Error(t("common.required"));
    if (name !== action.admin.display_name) body.display_name = name;
    if (editRole !== action.admin.role) body.role = editRole;
    if (Object.keys(body).length) {
      await update.mutateAsync({ adminId: action.admin.id, body });
      toast.ok(t("admin.updated"));
    }
    setAction(null);
  };

  const confirmAction = async () => {
    if (!action) return;
    const a = action.admin;
    switch (action.kind) {
      case "deactivate":
      case "activate": {
        await update.mutateAsync({ adminId: a.id, body: { active: action.kind === "activate" } });
        toast.ok(t("admin.updated"));
        break;
      }
      case "reset": {
        if (!passwordValid(newPassword)) throw new Error(t("admin.password.invalid"));
        await reset.mutateAsync({ adminId: a.id, password: newPassword });
        toast.ok(t("admin.passwordReset"), t("admin.tokensRevoked"));
        break;
      }
      case "revoke": {
        await revoke.mutateAsync({ adminId: a.id });
        toast.ok(t("admin.tokensRevoked"));
        break;
      }
      case "edit":
        return;
    }
    setAction(null);
  };

  const editSelf = action?.kind === "edit" && action.admin.id === me?.id;
  const editLast = action?.kind === "edit" && lastSuperadmin(action.admin);

  return (
    <div className="flex flex-col gap-4">
      {methods.data ? (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
          <Callout tone={methods.data.password_login ? "ok" : "warn"} title={methods.data.password_login ? t("admin.passwordLogin.enabled") : t("admin.passwordLogin.disabled")}>
            {t("admin.passwordLogin.desc")}
          </Callout>
          <Callout tone={methods.data.oidc ? "ok" : "neutral"} title={t("admin.oidc")}>
            {methods.data.oidc ? (
              <span className="inline-flex items-center gap-1.5 flex-wrap">
                {t("admin.oidc.enabled", { issuer: "" })}
                <Mono className="text-xs">{methods.data.oidc.issuer}</Mono>
              </span>
            ) : (
              t("admin.oidc.disabled")
            )}
          </Callout>
        </div>
      ) : null}

      <Toolbar
        end={
          <Button size="sm" onClick={() => setCreating(true)}>
            <Plus className="size-3.5" />
            {t("admin.new")}
          </Button>
        }
      >
        <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder={t("common.search")} className="w-64" />
        <Checkbox checked={showInactive} onChange={(e) => setShowInactive(e.target.checked)} label={inactive ? `${t("admin.showInactive")} (${inactive})` : t("admin.showInactive")} />
      </Toolbar>

      <Card>
        <DataTable
          rows={rows}
          columns={columns}
          rowKey={(a) => a.id}
          loading={admins.isPending}
          error={admins.isError ? <QueryError error={admins.error} onRetry={() => void admins.refetch()} compact /> : undefined}
          empty={<EmptyState compact icon={<Users className="size-5" />} title={list.length ? t("common.noResults") : t("admin.empty")} />}
          sort={sort}
          onSortChange={setSort}
        />
      </Card>

      <FormDialog
        open={creating}
        onOpenChange={(o) => {
          setCreating(o);
          if (!o) setForm(EMPTY_CREATE);
        }}
        title={t("admin.new")}
        submitLabel={t("common.create")}
        onSubmit={submitCreate}
      >
        <div className="flex flex-col gap-3">
          <Field label={t("admin.email")} htmlFor="admin-email" required>
            <Input id="admin-email" type="email" autoComplete="off" value={form.email} onChange={(e) => setForm({ ...form, email: e.target.value })} />
          </Field>
          <Field label={t("admin.displayName")} htmlFor="admin-name" required>
            <Input id="admin-name" value={form.display_name} onChange={(e) => setForm({ ...form, display_name: e.target.value })} />
          </Field>
          <RoleField value={form.role} onChange={(role) => setForm({ ...form, role })} />
          <Field label={t("admin.password")} htmlFor="admin-password" hint={t("admin.password.hint")} required>
            <Input id="admin-password" type="password" autoComplete="new-password" value={form.password} onChange={(e) => setForm({ ...form, password: e.target.value })} />
          </Field>
          {methods.data && !methods.data.password_login ? <Callout tone="warn">{t("admin.passwordLogin.disabled.create")}</Callout> : null}
        </div>
      </FormDialog>

      <FormDialog
        open={action?.kind === "edit"}
        onOpenChange={(o) => !o && setAction(null)}
        title={t("admin.edit")}
        description={action?.admin.email}
        submitLabel={t("common.save")}
        onSubmit={submitEdit}
      >
        <div className="flex flex-col gap-3">
          <Field label={t("admin.displayName")} htmlFor="admin-edit-name" required>
            <Input id="admin-edit-name" value={editName} onChange={(e) => setEditName(e.target.value)} />
          </Field>
          <RoleField id="admin-edit-role" value={editRole} onChange={setEditRole} disabled={editSelf || editLast} />
          {editSelf ? <Callout tone="neutral">{t("admin.self.locked")}</Callout> : editLast ? <Callout tone="warn">{t("admin.lastSuperadmin")}</Callout> : null}
        </div>
      </FormDialog>

      <ConfirmDialog
        open={action?.kind === "deactivate" || action?.kind === "activate"}
        onOpenChange={(o) => !o && setAction(null)}
        title={action?.kind === "activate" ? t("admin.activate") : t("admin.deactivate")}
        description={action?.kind === "deactivate" ? t("admin.deactivate.desc", { email: action.admin.email }) : action?.kind === "activate" ? t("admin.activate.desc", { email: action.admin.email }) : undefined}
        confirmLabel={action?.kind === "activate" ? t("admin.activate") : t("admin.deactivate")}
        variant={action?.kind === "deactivate" ? "danger" : "primary"}
        onConfirm={confirmAction}
      />

      <ConfirmDialog
        open={action?.kind === "reset"}
        onOpenChange={(o) => !o && setAction(null)}
        title={t("admin.resetPassword")}
        description={t("admin.resetPassword.desc")}
        confirmLabel={t("admin.resetPassword")}
        onConfirm={confirmAction}
        disabled={!passwordValid(newPassword)}
      >
        <Field label={t("auth.newPassword")} htmlFor="admin-reset-password" hint={t("auth.newPassword.hint")}>
          <Input id="admin-reset-password" type="password" autoComplete="new-password" value={newPassword} onChange={(e) => setNewPassword(e.target.value)} autoFocus />
        </Field>
      </ConfirmDialog>

      <ConfirmDialog
        open={action?.kind === "revoke"}
        onOpenChange={(o) => !o && setAction(null)}
        title={t("admin.revokeTokens")}
        description={action ? t("admin.revokeTokens.desc", { email: action.admin.email }) : undefined}
        confirmLabel={t("admin.revokeTokens")}
        variant="danger"
        onConfirm={confirmAction}
      />
    </div>
  );
}
