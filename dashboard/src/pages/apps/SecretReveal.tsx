import { KeyRound } from "lucide-react";
import type { ReactNode } from "react";

import { useT } from "@/i18n";
import { Button } from "@/ui/Button";
import { Dialog } from "@/ui/Dialog";
import { Callout, CodeBlock, KV } from "@/ui/Primitives";

/** One-time display of a freshly issued secret (API key, webhook signing secret). */
export function SecretReveal({
  secret,
  title,
  notice,
  meta,
  onClose,
}: {
  secret: string | null;
  title: ReactNode;
  notice: ReactNode;
  meta?: Array<{ k: ReactNode; v: ReactNode }>;
  onClose: () => void;
}) {
  const t = useT();
  return (
    <Dialog
      open={secret !== null}
      onOpenChange={(o) => {
        if (!o) onClose();
      }}
      title={
        <span className="inline-flex items-center gap-2">
          <KeyRound className="size-4 text-fg-muted" />
          {title}
        </span>
      }
      footer={
        <Button variant="primary" onClick={onClose}>
          {t("common.close")}
        </Button>
      }
    >
      <div className="flex flex-col gap-3">
        <Callout tone="warn">{notice}</Callout>
        {secret ? <CodeBlock value={secret} maxHeight="6rem" /> : null}
        {meta && meta.length > 0 ? <KV items={meta} cols={2} /> : null}
      </div>
    </Dialog>
  );
}
