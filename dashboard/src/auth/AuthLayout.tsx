import { Outlet } from "@tanstack/react-router";
import { Languages, Moon, Sun } from "lucide-react";

import { useI18n } from "@/i18n";
import { useTheme } from "@/theme";
import { Button } from "@/ui/Button";

import { Logo } from "@/shell/Logo";

export function AuthLayout() {
  const { t, locale, setLocale } = useI18n();
  const { resolved, setTheme } = useTheme();
  return (
    <div className="min-h-dvh flex flex-col">
      <header className="flex items-center justify-between px-6 h-14">
        <div className="flex items-center gap-2.5">
          <Logo className="size-6" />
          <span className="text-[13px] font-semibold tracking-tight">{t("app.name")}</span>
          <span className="text-[13px] text-fg-faint">{t("app.tagline")}</span>
        </div>
        <div className="flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => setLocale(locale === "ru" ? "en" : "ru")} aria-label={t("nav.language")}>
            <Languages className="size-3.5" />
            {locale.toUpperCase()}
          </Button>
          <Button variant="ghost" size="icon" onClick={() => setTheme(resolved === "dark" ? "light" : "dark")} aria-label={t("nav.theme")}>
            {resolved === "dark" ? <Sun className="size-4" /> : <Moon className="size-4" />}
          </Button>
        </div>
      </header>
      <main className="flex-1 flex items-start justify-center px-4 pt-[10vh] pb-10">
        <div className="w-full max-w-sm animate-fade-in">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
