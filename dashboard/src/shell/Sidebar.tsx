import { Link } from "@tanstack/react-router";
import {
  Activity,
  BarChart3,
  Boxes,
  Disc3,
  FileCog,
  LayoutDashboard,
  Radio,
  Server,
  ShieldAlert,
  Users,
} from "lucide-react";
import type { ReactNode } from "react";

import type { T } from "@/api/client";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n, type MessageKey } from "@/i18n";
import { cn } from "@/lib/cn";

import { Logo } from "./Logo";

interface NavItem {
  to: "/" | "/nodes" | "/apps" | "/live" | "/moderation" | "/recordings" | "/analytics" | "/admin" | "/config";
  label: MessageKey;
  icon: ReactNode;
  perm?: T.AdminPermission;
  exact?: boolean;
}

const SECTIONS: Array<{ label: MessageKey; items: NavItem[] }> = [
  {
    label: "nav.section.platform",
    items: [
      { to: "/", label: "nav.overview", icon: <LayoutDashboard className="size-4" />, exact: true },
      { to: "/nodes", label: "nav.nodes", icon: <Server className="size-4" />, perm: "nodes:read" },
      { to: "/apps", label: "nav.apps", icon: <Boxes className="size-4" />, perm: "apps:read" },
    ],
  },
  {
    label: "nav.section.application",
    items: [
      { to: "/live", label: "nav.live", icon: <Radio className="size-4" /> },
      { to: "/moderation", label: "nav.moderation", icon: <ShieldAlert className="size-4" />, perm: "moderation:read" },
      { to: "/recordings", label: "nav.recordings", icon: <Disc3 className="size-4" /> },
      { to: "/analytics", label: "nav.analytics", icon: <BarChart3 className="size-4" />, perm: "analytics:read" },
    ],
  },
  {
    label: "nav.section.system",
    items: [
      { to: "/admin", label: "nav.admin", icon: <Users className="size-4" /> },
      { to: "/config", label: "nav.config", icon: <FileCog className="size-4" />, perm: "config:read" },
    ],
  },
];

export function Sidebar({ className, onNavigate }: { className?: string; onNavigate?: () => void }) {
  const { t } = useI18n();
  const { can } = useAuth();
  return (
    <aside className={cn("w-60 shrink-0 flex-col border-r border-border bg-surface", className)}>
      <div className="h-14 flex items-center gap-2.5 px-5 border-b border-border">
        <Logo className="size-6" />
        <div className="leading-tight">
          <div className="text-[13px] font-semibold tracking-tight">{t("app.name")}</div>
          <div className="text-[11px] text-fg-faint">{t("app.tagline")}</div>
        </div>
      </div>
      <nav className="flex-1 overflow-y-auto subtle-scroll px-3 py-4 space-y-5" aria-label={t("nav.menu")}>
        {SECTIONS.map((section) => {
          const items = section.items.filter((i) => !i.perm || can(i.perm));
          if (items.length === 0) return null;
          return (
            <div key={section.label}>
              <div className="px-2 mb-1.5 text-[11px] font-medium uppercase tracking-wider text-fg-faint">{t(section.label)}</div>
              <ul className="space-y-0.5">
                {items.map((item) => (
                  <li key={item.to}>
                    <Link
                      to={item.to}
                      onClick={onNavigate}
                      activeOptions={{ exact: item.exact ?? false }}
                      className="flex items-center gap-2.5 h-8 px-2 rounded-md text-[13px] text-fg-muted hover:text-fg hover:bg-surface-2 transition-colors"
                      activeProps={{ className: "bg-surface-2 text-fg font-medium", "aria-current": "page" }}
                    >
                      <span className="text-fg-faint [a[aria-current]_&]:text-fg" aria-hidden>
                        {item.icon}
                      </span>
                      {t(item.label)}
                    </Link>
                  </li>
                ))}
              </ul>
            </div>
          );
        })}
      </nav>
      <div className="px-5 py-3 border-t border-border text-[11px] text-fg-faint">
        <Activity className="inline size-3 mr-1 -mt-px" aria-hidden />
        <span className="tabular">{__DASHBOARD_VERSION__}</span>
      </div>
    </aside>
  );
}
