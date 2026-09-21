import { cn } from "@/lib/cn";

/** Monochrome mark: three rounded bars, a nod to a level meter. */
export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" className={cn("text-fg", className)} aria-hidden fill="currentColor">
      <rect x="3" y="9" width="4" height="6" rx="2" />
      <rect x="10" y="4" width="4" height="16" rx="2" />
      <rect x="17" y="7" width="4" height="10" rx="2" />
    </svg>
  );
}
