import { Search } from "lucide-react";
import { useEffect, useState } from "react";

import { useUsersQuery } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { cn } from "@/lib/cn";
import { Input } from "@/ui/Input";
import { Mono, Spinner } from "@/ui/Primitives";

import { isUuid } from "./model";

export { isUuid } from "./model";


function useDebounced<V>(value: V, ms: number): V {
  const [v, setV] = useState(value);
  useEffect(() => {
    const h = setTimeout(() => setV(value), ms);
    return () => clearTimeout(h);
  }, [value, ms]);
  return v;
}

/**
 * User id input with lookup: typing a name or external id lists matches (`users:read`), picking one
 * fills the id. A pasted UUID is accepted as is.
 */
export function UserPicker({
  value,
  onChange,
  autoFocus,
  placeholder,
  exclude,
}: {
  value: string;
  onChange: (id: string) => void;
  autoFocus?: boolean;
  placeholder?: string;
  exclude?: string;
}) {
  const { t } = useI18n();
  const { canApp } = useAuth();
  const [open, setOpen] = useState(false);
  const q = useDebounced(value.trim(), 250);
  const lookup = q.length >= 2 && !isUuid(q) && canApp("users:read");
  const users = useUsersQuery({ q, per_page: 8 }, lookup);
  const matches = (users.data ?? []).filter((u) => u.id !== exclude);

  return (
    <div className="relative">
      <div className="relative">
        <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-fg-faint" />
        <Input
          value={value}
          onChange={(e) => {
            onChange(e.target.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          onBlur={() => setTimeout(() => setOpen(false), 120)}
          autoFocus={autoFocus}
          placeholder={placeholder ?? t("moderation.userPicker.placeholder")}
          className="pl-8 font-mono text-[12.5px]"
          spellCheck={false}
        />
        {lookup && users.isFetching ? <Spinner className="absolute right-2.5 top-1/2 size-3.5 -translate-y-1/2" /> : null}
      </div>
      {open && lookup ? (
        <div className="absolute z-20 mt-1 w-full overflow-hidden rounded-xl border border-border bg-surface shadow-sm">
          {matches.length === 0 ? (
            <div className="px-3 py-2 text-[12.5px] text-fg-muted">{users.isFetching ? t("common.loading") : t("moderation.userPicker.none")}</div>
          ) : (
            <ul className="max-h-64 overflow-y-auto subtle-scroll py-1">
              {matches.map((u) => (
                <li key={u.id}>
                  <button
                    type="button"
                    className={cn("flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-[13px] hover:bg-surface-2", u.is_banned && "text-fg-muted")}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => {
                      onChange(u.id);
                      setOpen(false);
                    }}
                  >
                    <span className="truncate">
                      {u.display_name || u.external_id}
                      <span className="ml-2 text-fg-faint">{u.external_id}</span>
                    </span>
                    <Mono className="text-[11px] text-fg-faint">{u.id.slice(0, 8)}</Mono>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : null}
    </div>
  );
}
