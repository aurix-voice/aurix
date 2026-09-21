import { useNavigate } from "@tanstack/react-router";
import { Languages, LogOut, Monitor, Moon, Sun, UserRound } from "lucide-react";

import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { useTheme, type Theme } from "@/theme";
import { Menu, type MenuItem } from "@/ui/Menu";

function initials(name: string | null | undefined, email: string): string {
  const src = (name && name.trim()) || email;
  const parts = src.split(/[\s@._-]+/).filter(Boolean);
  return ((parts[0]?.[0] ?? "") + (parts[1]?.[0] ?? "")).toUpperCase() || "A";
}

export function ProfileMenu() {
  const { t, locale, setLocale } = useI18n();
  const { admin, signOut } = useAuth();
  const { theme, setTheme } = useTheme();
  const navigate = useNavigate();
  if (!admin) return null;

  const themeItem = (value: Theme, icon: React.ReactNode, label: string): MenuItem => ({
    label,
    icon,
    checked: theme === value,
    onSelect: () => setTheme(value),
  });

  const items: MenuItem[] = [
    {
      label: (
        <div className="leading-tight">
          <div className="font-medium truncate">{admin.display_name || admin.email}</div>
          <div className="text-[11px] text-fg-faint truncate">
            {admin.email} · {t(`role.${admin.role}`)}
          </div>
        </div>
      ),
      disabled: true,
    },
    {
      label: t("nav.profile"),
      icon: <UserRound className="size-3.5" />,
      separatorBefore: true,
      onSelect: () => void navigate({ to: "/admin", search: { tab: "profile" } }),
    },
    {
      label: `${t("nav.language")}: ${locale === "ru" ? "Русский" : "English"}`,
      icon: <Languages className="size-3.5" />,
      separatorBefore: true,
      onSelect: () => setLocale(locale === "ru" ? "en" : "ru"),
    },
    { ...themeItem("light", <Sun className="size-3.5" />, t("nav.theme.light")), separatorBefore: true },
    themeItem("dark", <Moon className="size-3.5" />, t("nav.theme.dark")),
    themeItem("system", <Monitor className="size-3.5" />, t("nav.theme.system")),
    {
      label: t("nav.signOut"),
      icon: <LogOut className="size-3.5" />,
      separatorBefore: true,
      danger: true,
      onSelect: () => signOut("manual"),
    },
  ];

  return (
    <Menu
      items={items}
      label={t("nav.profile")}
      trigger={
        <button
          type="button"
          className="size-8 rounded-full border border-border bg-surface-2 text-[11px] font-semibold text-fg-muted hover:text-fg hover:border-border-strong transition-colors"
          aria-label={t("nav.profile")}
          title={admin.email}
        >
          {initials(admin.display_name, admin.email)}
        </button>
      }
    />
  );
}

