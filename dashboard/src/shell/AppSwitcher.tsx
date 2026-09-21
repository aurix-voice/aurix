import * as Popover from "@radix-ui/react-popover";
import { Link } from "@tanstack/react-router";
import { Check, ChevronsUpDown, Search, Settings2 } from "lucide-react";
import { useMemo, useState } from "react";

import { useAppsQuery, useSelectedApp } from "@/api/hooks";
import { useAppScope } from "@/api/scope";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { cn } from "@/lib/cn";
import { Badge } from "@/ui/Primitives";

export function AppSwitcher() {
  const { t } = useI18n();
  const { can } = useAuth();
  const { appId, setAppId } = useAppScope();
  const apps = useAppsQuery();
  const selected = useSelectedApp();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");

  const list = useMemo(() => {
    const all = apps.data ?? [];
    const needle = q.trim().toLowerCase();
    const filtered = needle ? all.filter((a) => a.name.toLowerCase().includes(needle) || a.id.startsWith(needle)) : all;
    return [...filtered].sort((a, b) => Number(b.active) - Number(a.active) || a.name.localeCompare(b.name));
  }, [apps.data, q]);

  if (!can("apps:read")) return null;

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-2 h-8 max-w-[260px] rounded-md border border-border bg-surface px-2.5 text-[13px] hover:bg-surface-2 transition-colors"
          aria-label={t("nav.appSwitcher")}
        >
          <span className="text-fg-faint text-[11px] uppercase tracking-wide hidden sm:inline">{t("nav.appSwitcher")}</span>
          <span className={cn("truncate", !selected && "text-fg-muted")}>
            {selected ? selected.name : appId ? appId.slice(0, 8) : t("nav.appSwitcher.none")}
          </span>
          {selected && !selected.active ? <Badge tone="warn">{t("common.inactive")}</Badge> : null}
          <ChevronsUpDown className="size-3.5 text-fg-faint shrink-0" aria-hidden />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={6}
          className="z-50 w-[320px] rounded-lg border border-border bg-surface shadow-lg outline-none animate-fade-in"
        >
          <div className="flex items-center gap-2 px-3 h-10 border-b border-border">
            <Search className="size-3.5 text-fg-faint" aria-hidden />
            <input
              autoFocus
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={t("nav.appSwitcher.search")}
              className="flex-1 bg-transparent text-[13px] outline-none placeholder:text-fg-faint"
            />
          </div>
          <ul className="max-h-72 overflow-y-auto subtle-scroll p-1" role="listbox">
            {list.length === 0 ? (
              <li className="px-2 py-6 text-center text-[12px] text-fg-faint">{apps.isPending ? t("common.loading") : t("common.noResults")}</li>
            ) : (
              list.map((a) => (
                <li key={a.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={a.id === appId}
                    onClick={() => {
                      setAppId(a.id);
                      setOpen(false);
                      setQ("");
                    }}
                    className="w-full flex items-center gap-2 h-8 px-2 rounded-md text-left text-[13px] hover:bg-surface-2"
                  >
                    <span className={cn("size-3.5 flex items-center justify-center", a.id !== appId && "invisible")}>
                      <Check className="size-3.5" aria-hidden />
                    </span>
                    <span className="truncate flex-1">{a.name}</span>
                    {!a.active ? <Badge tone="warn">{t("common.inactive")}</Badge> : null}
                    <span className="font-mono text-[11px] text-fg-faint">{a.id.slice(0, 6)}</span>
                  </button>
                </li>
              ))
            )}
          </ul>
          <div className="border-t border-border p-1">
            <Link
              to="/apps"
              onClick={() => setOpen(false)}
              className="flex items-center gap-2 h-8 px-2 rounded-md text-[13px] text-fg-muted hover:bg-surface-2 hover:text-fg"
            >
              <Settings2 className="size-3.5" aria-hidden />
              {t("nav.appSwitcher.manage")}
            </Link>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
