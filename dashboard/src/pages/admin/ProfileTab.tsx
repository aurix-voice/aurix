import { KeyRound, LogOut } from "lucide-react";
import { useState } from "react";

import { errorMessage } from "@/api/client";
import { useAuthMethodsQuery, useChangeOwnPasswordMutation, useLogoutAllMutation, useSelectedApp } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { ALL_APP_PERMISSIONS } from "@/auth/store";
import { useI18n } from "@/i18n";
import { fmtDateTime } from "@/lib/format";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { Input } from "@/ui/Input";
import { Badge, Callout, Card, CardHeader, Field, KV, Mono } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { ALL_ADMIN_PERMISSIONS, passwordValid } from "./model";
import { RoleBadge, SourceBadge } from "./shared";

export function ProfileTab() {
  const { t, locale } = useI18n();
  const { admin, can, canApp, signOut, refreshAdmin } = useAuth();
  const app = useSelectedApp();
  const methods = useAuthMethodsQuery();
  const change = useChangeOwnPasswordMutation();
  const logoutAll = useLogoutAllMutation();
  const toast = useToast();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [repeat, setRepeat] = useState("");
  const [confirmLogout, setConfirmLogout] = useState(false);

  if (!admin) return null;

  const mismatch = repeat.length > 0 && next !== repeat;
  const canSubmit = current.length > 0 && passwordValid(next) && next === repeat && !change.isPending;
  const passwordAvailable = admin.has_password !== false && methods.data?.password_login !== false;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    await change.mutateAsync({ current_password: current, new_password: next });
    toast.ok(t("auth.passwordChanged"), t("auth.passwordChanged.desc"));
    setCurrent("");
    setNext("");
    setRepeat("");
    await refreshAdmin();
  };

  const doLogoutAll = async () => {
    await logoutAll.mutateAsync();
    signOut("manual");
  };

  return (
    <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
      <Card>
        <CardHeader title={admin.display_name} description={admin.email} actions={<RoleBadge role={admin.role} />} />
        <div className="px-4 pb-4 flex flex-col gap-4">
          <KV
            cols={2}
            items={[
              { k: t("common.id"), v: <Mono className="text-xs">{admin.id}</Mono> },
              { k: t("auth.source"), v: <SourceBadge admin={admin} /> },
              { k: t("admin.lastLogin"), v: fmtDateTime(locale, admin.last_login_at) },
              { k: t("common.created"), v: fmtDateTime(locale, admin.created_at) },
            ]}
          />
          <div>
            <div className="text-[11px] uppercase tracking-wide text-fg-faint font-medium mb-1.5">{t("admin.permissions.platform")}</div>
            <ul className="flex flex-wrap gap-1">
              {ALL_ADMIN_PERMISSIONS.map((p) => (
                <li key={p}>
                  <Badge tone={can(p) ? "ok" : "neutral"} className={can(p) ? "" : "opacity-50"}>
                    {t(`perm.${p}`)}
                  </Badge>
                </li>
              ))}
            </ul>
          </div>
          <div>
            <div className="text-[11px] uppercase tracking-wide text-fg-faint font-medium mb-1.5">
              {t("admin.permissions.app")}
              {app ? <span className="normal-case tracking-normal text-fg-muted"> · {app.name}</span> : null}
            </div>
            {app ? (
              <ul className="flex flex-wrap gap-1">
                {ALL_APP_PERMISSIONS.map((p) => (
                  <li key={p}>
                    <Badge tone={canApp(p) ? "accent" : "neutral"} className={canApp(p) ? "" : "opacity-50"}>
                      <span className="font-mono text-[11px]">{p}</span>
                    </Badge>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-xs text-fg-muted">{t("admin.permissions.noApp")}</p>
            )}
          </div>
        </div>
      </Card>

      <div className="flex flex-col gap-4">
        <Card>
          <CardHeader title={t("auth.changePassword")} description={passwordAvailable ? undefined : t("auth.changePassword.unavailable")} />
          <form className="px-4 pb-4 flex flex-col gap-3" onSubmit={(e) => void submit(e)}>
            <Field label={t("auth.currentPassword")} htmlFor="pw-current">
              <Input id="pw-current" type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} disabled={!passwordAvailable} />
            </Field>
            <Field label={t("auth.newPassword")} htmlFor="pw-next" hint={t("auth.newPassword.hint")}>
              <Input id="pw-next" type="password" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} disabled={!passwordAvailable} />
            </Field>
            <Field label={t("auth.repeatPassword")} htmlFor="pw-repeat" error={mismatch ? t("auth.passwordMismatch") : undefined}>
              <Input id="pw-repeat" type="password" autoComplete="new-password" value={repeat} onChange={(e) => setRepeat(e.target.value)} disabled={!passwordAvailable} />
            </Field>
            {change.isError ? <Callout tone="danger">{errorMessage(change.error, locale)}</Callout> : null}
            <div className="flex justify-end">
              <Button type="submit" size="sm" disabled={!canSubmit || !passwordAvailable} loading={change.isPending}>
                <KeyRound className="size-3.5" />
                {t("auth.changePassword")}
              </Button>
            </div>
          </form>
        </Card>

        <Card>
          <CardHeader
            title={t("auth.logoutAll")}
            description={t("auth.logoutAll.desc")}
            actions={
              <Button size="sm" variant="danger" onClick={() => setConfirmLogout(true)}>
                <LogOut className="size-3.5" />
                {t("auth.logoutAll")}
              </Button>
            }
          />
        </Card>
      </div>

      <ConfirmDialog
        open={confirmLogout}
        onOpenChange={setConfirmLogout}
        title={t("auth.logoutAll")}
        description={t("auth.logoutAll.confirm")}
        confirmLabel={t("auth.logoutAll")}
        variant="danger"
        onConfirm={doLogoutAll}
      />
    </div>
  );
}
