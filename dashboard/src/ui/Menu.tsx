import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import * as TabsPrimitive from "@radix-ui/react-tabs";
import { Check, MoreHorizontal } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/cn";

import { Button } from "./Button";

export interface MenuItem {
  label: ReactNode;
  icon?: ReactNode;
  onSelect?: () => void;
  danger?: boolean;
  disabled?: boolean;
  hidden?: boolean;
  separatorBefore?: boolean;
  checked?: boolean;
}

export function Menu({ items, trigger, align = "end", label }: { items: MenuItem[]; trigger?: ReactNode; align?: "start" | "end"; label?: string }) {
  const visible = items.filter((i) => !i.hidden);
  if (visible.length === 0) return null;
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger asChild>
        {trigger ?? (
          <Button variant="ghost" size="icon" className="h-7 w-7" aria-label={label ?? "actions"} data-testid="row-menu">
            <MoreHorizontal className="size-4" />
          </Button>
        )}
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align={align}
          sideOffset={4}
          className="z-50 min-w-44 rounded-md border border-border bg-surface p-1 animate-fade-in outline-none"
        >
          {visible.map((item, i) => (
            <div key={i}>
              {item.separatorBefore ? <DropdownMenu.Separator className="my-1 h-px bg-border" /> : null}
              <DropdownMenu.Item
                disabled={item.disabled}
                onSelect={() => item.onSelect?.()}
                className={cn(
                  "flex items-center gap-2 rounded px-2 py-1.5 text-[13px] outline-none cursor-default select-none",
                  "data-[highlighted]:bg-surface-2 data-[disabled]:opacity-40",
                  item.danger ? "text-danger" : "text-fg",
                )}
              >
                {item.icon ? <span className="text-fg-muted [&>svg]:size-3.5">{item.icon}</span> : null}
                <span className="flex-1">{item.label}</span>
                {item.checked ? <Check className="size-3.5" /> : null}
              </DropdownMenu.Item>
            </div>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

// ---- Tabs (underline style)
export function Tabs({ value, onValueChange, tabs, className, children }: { value: string; onValueChange: (v: string) => void; tabs: Array<{ value: string; label: ReactNode; count?: number; hidden?: boolean }>; className?: string; children?: ReactNode }) {
  return (
    <TabsPrimitive.Root value={value} onValueChange={onValueChange} className={className}>
      <TabsPrimitive.List className="flex items-center gap-1 border-b border-border overflow-x-auto subtle-scroll">
        {tabs
          .filter((t) => !t.hidden)
          .map((t) => (
            <TabsPrimitive.Trigger
              key={t.value}
              value={t.value}
              className="relative -mb-px h-9 px-3 text-[13px] text-fg-muted whitespace-nowrap data-[state=active]:text-fg data-[state=active]:font-medium border-b-2 border-transparent data-[state=active]:border-fg transition-colors outline-none hover:text-fg"
            >
              {t.label}
              {t.count !== undefined ? <span className="ml-1.5 text-[11px] text-fg-faint tabular">{t.count}</span> : null}
            </TabsPrimitive.Trigger>
          ))}
      </TabsPrimitive.List>
      {children}
    </TabsPrimitive.Root>
  );
}

export const TabPanel = TabsPrimitive.Content;

/** Segmented control for small option sets. */
export function Segmented<V extends string>({ value, onChange, options, className, size = "md" }: { value: V; onChange: (v: V) => void; options: Array<{ value: V; label: ReactNode }>; className?: string; size?: "sm" | "md" }) {
  return (
    <div className={cn("inline-flex items-center rounded-md border border-border bg-surface-2 p-0.5", className)} role="tablist">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="tab"
          aria-selected={o.value === value}
          onClick={() => onChange(o.value)}
          className={cn(
            "rounded-[5px] font-medium transition-colors whitespace-nowrap",
            size === "sm" ? "h-6 px-2 text-[11px]" : "h-7 px-2.5 text-xs",
            o.value === value ? "bg-surface text-fg border border-border" : "text-fg-muted hover:text-fg border border-transparent",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
