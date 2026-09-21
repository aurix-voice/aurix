import { Play } from "lucide-react";
import { useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useEffectiveConfigQuery, useRetentionSweepMutation } from "@/api/hooks";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtNumber } from "@/lib/format";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { Callout, Card, CardHeader, Mono, Stat } from "@/ui/Primitives";

import { sweepTotal } from "./model";

const REPORT_KEYS = ["sessions", "moderation_events", "audit_log", "analytics", "tombstones", "inactive_users"] as const;

export function RetentionTab() {
  const { t, locale } = useI18n();
  const sweep = useRetentionSweepMutation();
  const config = useEffectiveConfigQuery();
  const [confirm, setConfirm] = useState(false);
  const [last, setLast] = useState<{ report: T.SweepReport; at: string } | null>(null);

  const run = async () => {
    const report = await sweep.mutateAsync();
    setLast({ report, at: new Date().toISOString() });
    setConfirm(false);
  };

  return (
    <div className="flex flex-col gap-4 max-w-3xl">
      <Card>
        <CardHeader
          title={t("admin.retention.title")}
          description={t("admin.retention.desc")}
          actions={
            <Button size="sm" onClick={() => setConfirm(true)} disabled={sweep.isPending}>
              <Play className="size-3.5" />
              {t("admin.retention.run")}
            </Button>
          }
        />
        <div className="px-4 pb-4 text-xs text-fg-muted">
          {config.data ? (
            <span>
              {t("admin.retention.node")}: <Mono>{config.data.node_id}</Mono>
            </span>
          ) : null}
        </div>
      </Card>

      {sweep.isError ? <Callout tone="danger" title={t("admin.retention.failed")}>{errorMessage(sweep.error, locale)}</Callout> : null}

      {last ? (
        <Card>
          <CardHeader title={t("admin.retention.result")} description={`${fmtDateTime(locale, last.at)} · ${t("admin.retention.removed", { n: sweepTotal(last.report) })}`} />
          <div className="px-4 pb-4 grid grid-cols-2 md:grid-cols-3 gap-3">
            {REPORT_KEYS.map((k) => (
              <Stat key={k} label={t(`admin.retention.${k}`)} value={fmtNumber(locale, last.report[k] ?? 0)} />
            ))}
          </div>
        </Card>
      ) : (
        <Callout tone="neutral">{t("admin.retention.noRun")}</Callout>
      )}

      <ConfirmDialog
        open={confirm}
        onOpenChange={setConfirm}
        title={t("admin.retention.run")}
        description={t("admin.retention.confirm")}
        confirmLabel={t("admin.retention.run")}
        variant="danger"
        onConfirm={run}
      />
    </div>
  );
}
