import { useAuth } from "@/auth/AuthProvider";
import type { AppPermission } from "@/auth/store";
import { useI18n } from "@/i18n";
import { Forbidden, RequireApp } from "@/shell/Guards";
import { Tabs } from "@/ui/Menu";
import { PageHeader } from "@/ui/Page";

import { BansTab } from "./BansTab";
import { ChatTab } from "./ChatTab";
import { EventsTab } from "./EventsTab";
import { MOD_TABS, useModSearch, type ModTab } from "./shared";
import { UsersTab } from "./UsersTab";

const TAB_PERM: Record<ModTab, AppPermission> = {
  events: "moderation:read",
  incidents: "moderation:read",
  bans: "moderation:read",
  users: "users:read",
  chat: "chat:read",
};

function Moderation() {
  const { t } = useI18n();
  const { canApp } = useAuth();
  const { tab, go } = useModSearch();
  const allowed = canApp(TAB_PERM[tab]);

  return (
    <>
      <PageHeader title={t("moderation.title")} description={t("moderation.subtitle")} />
      <Tabs
        value={tab}
        onValueChange={(v) => go({ tab: v as ModTab, event: null }, false)}
        tabs={MOD_TABS.map((value) => ({ value, label: t(`moderation.tabs.${value}`), hidden: !canApp(TAB_PERM[value]) }))}
      />
      <div className="pt-4">
        {!allowed ? (
          <Forbidden perm={TAB_PERM[tab]} />
        ) : tab === "events" ? (
          <EventsTab mode="events" />
        ) : tab === "incidents" ? (
          <EventsTab mode="incidents" />
        ) : tab === "bans" ? (
          <BansTab />
        ) : tab === "users" ? (
          <UsersTab />
        ) : (
          <ChatTab />
        )}
      </div>
    </>
  );
}

export default function ModerationPage() {
  return <RequireApp>{() => <Moderation />}</RequireApp>;
}
