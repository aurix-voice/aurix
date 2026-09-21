import { Outlet, useNavigate, useRouterState } from "@tanstack/react-router";
import { Menu as MenuIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { Button } from "@/ui/Button";

import { AppSwitcher } from "./AppSwitcher";
import { LiveIndicator } from "./LiveIndicator";
import { ProfileMenu } from "./ProfileMenu";
import { Sidebar } from "./Sidebar";

export function Shell() {
  const { status } = useAuth();
  const { t } = useI18n();
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const [drawer, setDrawer] = useState(false);

  useEffect(() => {
    if (status === "anonymous") {
      void navigate({ to: "/login", search: { redirect: pathname === "/" ? undefined : pathname }, replace: true });
    }
  }, [status, navigate, pathname]);

  if (status !== "authenticated") return null;

  return (
    <div className="min-h-dvh flex">
      <Sidebar className="hidden lg:flex" />
      {drawer ? (
        <div className="fixed inset-0 z-40 lg:hidden">
          <button type="button" className="absolute inset-0 bg-black/30" aria-label={t("common.close")} onClick={() => setDrawer(false)} />
          <Sidebar className="relative z-10 h-full shadow-xl" onNavigate={() => setDrawer(false)} />
        </div>
      ) : null}
      <div className="flex-1 min-w-0 flex flex-col">
        <header className="sticky top-0 z-30 h-14 shrink-0 border-b border-border bg-bg/85 backdrop-blur flex items-center gap-2 px-3 sm:px-5">
          <Button variant="ghost" size="icon" className="lg:hidden" aria-label={t("nav.menu")} onClick={() => setDrawer(true)}>
            <MenuIcon className="size-4" />
          </Button>
          <AppSwitcher />
          <div className="flex-1" />
          <LiveIndicator />
          <ProfileMenu />
        </header>
        <main className="flex-1 min-w-0 px-4 sm:px-6 lg:px-8 py-6 max-w-[1400px] w-full mx-auto animate-fade-in">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
