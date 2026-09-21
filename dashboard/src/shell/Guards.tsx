import { Boxes, Lock } from "lucide-react";
import type { ReactNode } from "react";

import type { T } from "@/api/client";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { EmptyState } from "@/ui/Primitives";

export function Forbidden({ perm }: { perm?: T.AdminPermission }) {
  const { t } = useI18n();
  return (
    <EmptyState
      icon={<Lock className="size-5" />}
      title={t("common.forbidden")}
      description={
        <>
          {t("common.forbidden.desc")}
          {perm ? <div className="mt-1 font-mono text-[11px]">{t("common.forbidden.perm", { perm })}</div> : null}
        </>
      }
    />
  );
}

export function RequirePermission({ perm, children }: { perm: T.AdminPermission; children: ReactNode }) {
  const { can } = useAuth();
  if (!can(perm)) return <Forbidden perm={perm} />;
  return <>{children}</>;
}

/** Tenant pages need a selected application; the switcher in the top bar sets it. */
export function RequireApp({ children }: { children: (appId: string) => ReactNode }) {
  const { t } = useI18n();
  const { appId } = useAppScope();
  if (!appId) {
    return <EmptyState icon={<Boxes className="size-5" />} title={t("common.selectApp")} description={t("common.selectApp.desc")} />;
  }
  return <>{children(appId)}</>;
}
