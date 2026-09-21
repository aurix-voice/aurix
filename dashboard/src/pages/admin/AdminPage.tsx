import { useNavigate } from "@tanstack/react-router";

import type { T } from "@/api/client";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { adminRoute } from "@/router";
import { Tabs } from "@/ui/Menu";
import { PageHeader } from "@/ui/Page";

import { AdminsTab } from "./AdminsTab";
import { AuditTab } from "./AuditTab";
import { ADMIN_TABS, parseAdminTab, type AdminTab } from "./model";
import { ProfileTab } from "./ProfileTab";
import { RetentionTab } from "./RetentionTab";

const TAB_PERM: Record<AdminTab, T.AdminPermission | null> = {
  admins: "admins:manage",
  audit: "audit:read",
  retention: "retention:run",
  profile: null,
};

export default function AdminPage() {
  const { t } = useI18n();
  const { can } = useAuth();
  const search = adminRoute.useSearch();
  const navigate = useNavigate();

  const allowed = ADMIN_TABS.filter((tab) => {
    const perm = TAB_PERM[tab];
    return perm === null || can(perm);
  });
  const tab = parseAdminTab(search.tab, allowed);

  return (
    <>
      <PageHeader title={t("admin.title")} description={t("admin.subtitle")} />
      <Tabs
        value={tab}
        onValueChange={(v) => void navigate({ to: "/settings", search: { tab: v }, replace: true })}
        tabs={ADMIN_TABS.map((value) => ({ value, label: t(`admin.tabs.${value}`), hidden: !allowed.includes(value) }))}
        className="flex flex-col gap-4"
      >
        {tab === "admins" ? <AdminsTab /> : tab === "audit" ? <AuditTab /> : tab === "retention" ? <RetentionTab /> : <ProfileTab />}
      </Tabs>
    </>
  );
}
