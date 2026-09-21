import * as DialogPrimitive from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { useState, type FormEvent, type ReactNode } from "react";

import { useT } from "@/i18n";
import { cn } from "@/lib/cn";

import { Button, type ButtonVariant } from "./Button";
import { Callout } from "./Primitives";

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  size?: "sm" | "md" | "lg" | "xl";
  className?: string;
}

const widths = { sm: "max-w-sm", md: "max-w-md", lg: "max-w-2xl", xl: "max-w-4xl" };

export function Dialog({ open, onOpenChange, title, description, children, footer, size = "md", className }: DialogProps) {
  const t = useT();
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="fixed inset-0 z-40 bg-black/30 dark:bg-black/60 animate-fade-in" />
        <DialogPrimitive.Content
          className={cn(
            "fixed left-1/2 top-[12vh] z-50 w-[calc(100vw-2rem)] -translate-x-1/2 rounded-lg border border-border bg-surface animate-fade-in outline-none flex flex-col max-h-[80vh]",
            widths[size],
            className,
          )}
        >
          <div className="flex items-start justify-between gap-4 px-5 pt-4 pb-3">
            <div className="min-w-0">
              <DialogPrimitive.Title className="text-[15px] font-semibold leading-6">{title}</DialogPrimitive.Title>
              {description ? (
                <DialogPrimitive.Description className="text-xs text-fg-muted mt-1 leading-relaxed">{description}</DialogPrimitive.Description>
              ) : (
                <DialogPrimitive.Description className="sr-only">{typeof title === "string" ? title : "dialog"}</DialogPrimitive.Description>
              )}
            </div>
            <DialogPrimitive.Close asChild>
              <Button variant="ghost" size="icon" className="-mr-2 -mt-1 h-7 w-7" aria-label={t("common.close")}>
                <X className="size-4" />
              </Button>
            </DialogPrimitive.Close>
          </div>
          {children ? <div className="px-5 pb-4 overflow-y-auto subtle-scroll flex-1 min-h-0">{children}</div> : null}
          {footer ? <div className="flex items-center justify-end gap-2 px-5 py-3 border-t border-border">{footer}</div> : null}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

export interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  confirmLabel?: ReactNode;
  variant?: ButtonVariant;
  onConfirm: () => Promise<unknown> | void;
  children?: ReactNode;
  disabled?: boolean;
}

/** Confirmation with an async action; the error from `onConfirm` is shown inline. */
export function ConfirmDialog({ open, onOpenChange, title, description, confirmLabel, variant = "primary", onConfirm, children, disabled }: ConfirmDialogProps) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      onOpenChange(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!busy) {
          setError(null);
          onOpenChange(o);
        }
      }}
      title={title}
      description={description}
      size="sm"
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            {t("common.cancel")}
          </Button>
          <Button variant={variant} onClick={() => void submit()} loading={busy} disabled={disabled} data-testid="confirm">
            {confirmLabel ?? t("common.confirm")}
          </Button>
        </>
      }
    >
      {children || error ? (
        <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
          {children}
          {error ? <Callout tone="danger">{error}</Callout> : null}
        </form>
      ) : null}
    </Dialog>
  );
}

/** Form dialog: submit on Enter, inline error, busy state. */
export function FormDialog({
  open,
  onOpenChange,
  title,
  description,
  submitLabel,
  onSubmit,
  children,
  size = "md",
  disabled,
  extraFooter,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  submitLabel?: ReactNode;
  onSubmit: () => Promise<unknown> | void;
  children: ReactNode;
  size?: "sm" | "md" | "lg" | "xl";
  disabled?: boolean;
  extraFooter?: ReactNode;
}) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const formId = "form-dialog";
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await onSubmit();
      onOpenChange(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!busy) {
          setError(null);
          onOpenChange(o);
        }
      }}
      title={title}
      description={description}
      size={size}
      footer={
        <>
          {extraFooter}
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            {t("common.cancel")}
          </Button>
          <Button variant="primary" type="submit" form={formId} loading={busy} disabled={disabled} data-testid="submit">
            {submitLabel ?? t("common.save")}
          </Button>
        </>
      }
    >
      <form id={formId} onSubmit={(e) => void submit(e)} className="flex flex-col gap-3.5">
        {children}
        {error ? <Callout tone="danger">{error}</Callout> : null}
      </form>
    </Dialog>
  );
}
