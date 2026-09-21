import { forwardRef, type InputHTMLAttributes, type SelectHTMLAttributes, type TextareaHTMLAttributes } from "react";

import { cn } from "@/lib/cn";

const base =
  "w-full bg-surface border border-border rounded-md text-fg placeholder:text-fg-faint disabled:opacity-50 disabled:bg-surface-2 transition-colors hover:border-border-strong focus:border-border-strong";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input(
  { className, ...rest },
  ref,
) {
  return <input ref={ref} className={cn(base, "h-8 px-2.5 text-[13px]", className)} {...rest} />;
});

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement>>(
  function Textarea({ className, ...rest }, ref) {
    return <textarea ref={ref} className={cn(base, "min-h-20 p-2.5 text-[13px] leading-snug", className)} {...rest} />;
  },
);

export const NativeSelect = forwardRef<HTMLSelectElement, SelectHTMLAttributes<HTMLSelectElement>>(
  function NativeSelect({ className, children, ...rest }, ref) {
    return (
      <select
        ref={ref}
        className={cn(
          base,
          "h-8 pl-2.5 pr-7 text-[13px] appearance-none bg-no-repeat bg-[right_0.5rem_center] bg-[length:14px_14px]",
          "bg-[url('data:image/svg+xml;utf8,<svg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 24 24%22 fill=%22none%22 stroke=%22%23888%22 stroke-width=%222%22 stroke-linecap=%22round%22 stroke-linejoin=%22round%22><path d=%22m6 9 6 6 6-6%22/></svg>')]",
          className,
        )}
        {...rest}
      >
        {children}
      </select>
    );
  },
);

export function Checkbox({
  className,
  label,
  ...rest
}: InputHTMLAttributes<HTMLInputElement> & { label?: string }) {
  return (
    <label className={cn("inline-flex items-center gap-2 text-[13px] select-none cursor-pointer", className)}>
      <input type="checkbox" className="size-3.5 accent-[var(--fg)] rounded" {...rest} />
      {label}
    </label>
  );
}
