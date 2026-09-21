import { MoreHorizontal, Plus, Webhook as WebhookIcon } from "lucide-react";
import { useCallback, useMemo, useState } from "react";

import type { T } from "@/api/client";
import { errorMessage } from "@/api/client";
import {
  useAppWebhooksQuery,
  useCreateWebhookMutation,
  useDeleteWebhookMutation,
  useEventTypesQuery,
  useResyncWebhookMutation,
  useRetryDeliveryMutation,
  useRotateWebhookSecretMutation,
  useTestWebhookMutation,
  useUpdateWebhookMutation,
  useWebhookDeliveriesQuery,
} from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { ConfirmDialog, Dialog, FormDialog } from "@/ui/Dialog";
import { Checkbox, Input, Textarea } from "@/ui/Input";
import { Menu, Segmented } from "@/ui/Menu";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, CodeBlock, EmptyState, Field, IdChip, Mono, Switch, Tip, type Tone } from "@/ui/Primitives";
import { DataTable, Pager, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { SecretReveal } from "./SecretReveal";

interface WebhookForm {
  url: string;
  description: string;
  allEvents: boolean;
  events: string[];
}

const EMPTY_FORM: WebhookForm = { url: "", description: "", allEvents: true, events: [] };

function statusTone(status: string): Tone {
  switch (status) {
    case "delivered":
      return "ok";
    case "failed":
      return "danger";
    case "retrying":
      return "warn";
    default:
      return "neutral";
  }
}

function httpTone(code: number | null | undefined): Tone {
  if (code == null) return "neutral";
  return code >= 200 && code < 300 ? "ok" : code >= 500 || code === 0 ? "danger" : "warn";
}

function DeliveryStatus({ status }: { status: string }) {
  const { t } = useI18n();
  const label =
    status === "delivered"
      ? t("webhooks.status.delivered")
      : status === "failed"
        ? t("webhooks.status.failed")
        : status === "retrying"
          ? t("webhooks.status.retrying")
          : status === "pending"
            ? t("webhooks.status.pending")
            : status;
  return (
    <Badge tone={statusTone(status)} dot>
      {label}
    </Badge>
  );
}

const PAGE = 25;
type DeliveryFilter = "all" | "failed" | "retrying" | "delivered" | "pending";

function DeliveriesDialog({ appId, webhook, onClose }: { appId: string; webhook: T.Webhook | null; onClose: () => void }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const toast = useToast();
  const now = useNow();
  const [filter, setFilter] = useState<DeliveryFilter>("all");
  const [offset, setOffset] = useState(0);
  const [payload, setPayload] = useState<T.WebhookDelivery | null>(null);
  const q = useWebhookDeliveriesQuery(appId, webhook?.id ?? null, { status: filter === "all" ? undefined : filter, limit: PAGE, offset });
  const retry = useRetryDeliveryMutation(appId);
  const canWrite = canApp("webhooks:write");

  const columns = useMemo<Column<T.WebhookDelivery>[]>(
    () => [
      {
        key: "event",
        header: t("webhooks.events"),
        cell: (d) => (
          <div className="flex flex-col">
            <Mono>{d.event_type}</Mono>
            <IdChip id={d.event_id} />
          </div>
        ),
      },
      { key: "status", header: t("common.status"), cell: (d) => <DeliveryStatus status={d.status} /> },
      {
        key: "http",
        header: t("webhooks.lastStatus"),
        cell: (d) => (
          <div className="flex flex-col">
            {d.last_status != null ? <Badge tone={httpTone(d.last_status)}>HTTP {d.last_status}</Badge> : <span className="text-fg-faint">—</span>}
            {d.last_error ? <span className="text-[11px] text-danger truncate max-w-xs" title={d.last_error}>{d.last_error}</span> : null}
          </div>
        ),
      },
      { key: "attempts", header: t("webhooks.delivery.attempts"), align: "right", cell: (d) => <span className="tabular">{d.attempts}</span> },
      {
        key: "created",
        header: t("common.created"),
        cell: (d) => (
          <Tip content={fmtDateTime(locale, d.created_at)}>
            <span className="text-fg-muted whitespace-nowrap">{fmtRelative(locale, d.created_at, now)}</span>
          </Tip>
        ),
      },
      {
        key: "next",
        header: t("webhooks.delivery.next"),
        cell: (d) => <span className="text-fg-muted whitespace-nowrap">{d.delivered_at ? fmtDateTime(locale, d.delivered_at) : d.next_attempt_at ? fmtRelative(locale, d.next_attempt_at, now) : "—"}</span>,
      },
      {
        key: "actions",
        header: "",
        align: "right",
        cell: (d) => (
          <div className="flex items-center justify-end gap-1">
            <Button size="xs" variant="ghost" onClick={() => setPayload(d)}>
              {t("webhooks.delivery.payload")}
            </Button>
            {canWrite && d.status !== "delivered" && webhook ? (
              <Button
                size="xs"
                variant="ghost"
                onClick={async () => {
                  try {
                    await retry.mutateAsync({ webhookId: webhook.id, deliveryId: d.id });
                    toast.ok(t("webhooks.delivery.retried"));
                    void q.refetch();
                  } catch (e) {
                    toast.error(t("common.error"), errorMessage(e, locale));
                  }
                }}
              >
                {t("webhooks.delivery.retry")}
              </Button>
            ) : null}
          </div>
        ),
      },
    ],
    [t, locale, now, canWrite, webhook, retry, toast, q],
  );

  const rows = q.data?.deliveries;
  return (
    <Dialog
      open={webhook !== null}
      onOpenChange={(o) => {
        if (!o) onClose();
      }}
      title={t("webhooks.deliveries")}
      description={webhook ? <Mono>{webhook.url}</Mono> : undefined}
      size="xl"
    >
      <div className="flex flex-col -mx-5 -mb-4">
        <Toolbar
          end={<Pager onPage={(dir) => setOffset((o) => Math.max(0, o + dir * PAGE))} hasPrev={offset > 0} hasNext={(rows?.length ?? 0) === PAGE} />}
        >
          <Segmented
            size="sm"
            value={filter}
            onChange={(v) => {
              setFilter(v);
              setOffset(0);
            }}
            options={[
              { value: "all", label: t("common.all") },
              { value: "failed", label: t("webhooks.status.failed") },
              { value: "retrying", label: t("webhooks.status.retrying") },
              { value: "pending", label: t("webhooks.status.pending") },
              { value: "delivered", label: t("webhooks.status.delivered") },
            ]}
          />
        </Toolbar>
        <DataTable
          rows={rows}
          columns={columns}
          rowKey={(d) => d.id}
          loading={q.isPending}
          dense
          error={q.isError ? <QueryError error={q.error} onRetry={() => void q.refetch()} compact /> : undefined}
          empty={<EmptyState compact title={t("webhooks.deliveries.empty")} />}
        />
      </div>
      <Dialog
        open={payload !== null}
        onOpenChange={(o) => {
          if (!o) setPayload(null);
        }}
        title={t("webhooks.delivery.payload")}
        description={payload ? <Mono>{payload.event_type}</Mono> : undefined}
        size="lg"
      >
        {payload ? <CodeBlock value={JSON.stringify(payload.payload, null, 2)} maxHeight="60vh" /> : null}
      </Dialog>
    </Dialog>
  );
}

function WebhookFormFields({ value, onChange, eventTypes }: { value: WebhookForm; onChange: (v: WebhookForm) => void; eventTypes: string[] | undefined }) {
  const { t } = useI18n();
  return (
    <div className="flex flex-col gap-3">
      <Field label={t("webhooks.url")} required hint={t("webhooks.url.hint")} htmlFor="wh-url">
        <Input id="wh-url" type="url" value={value.url} onChange={(e) => onChange({ ...value, url: e.target.value })} placeholder="https://" autoFocus required />
      </Field>
      <Field label={t("common.description")} hint={t("common.optional")} htmlFor="wh-desc">
        <Textarea id="wh-desc" rows={2} value={value.description} onChange={(e) => onChange({ ...value, description: e.target.value })} />
      </Field>
      <Field label={t("webhooks.events")}>
        <div className="flex flex-col gap-2">
          <Segmented<"all" | "custom">
            size="sm"
            value={value.allEvents ? "all" : "custom"}
            onChange={(v) => onChange({ ...value, allEvents: v === "all" })}
            options={[
              { value: "all", label: t("webhooks.events.all") },
              { value: "custom", label: t("keys.permissions.custom") },
            ]}
          />
          {!value.allEvents ? (
            <div className="grid grid-cols-2 gap-1.5 rounded-md border border-border p-2.5 max-h-64 overflow-auto subtle-scroll">
              {(eventTypes ?? []).map((ev) => (
                <Checkbox
                  key={ev}
                  label={ev}
                  className="font-mono text-[12px]"
                  checked={value.events.includes(ev)}
                  onChange={(e) => onChange({ ...value, events: e.target.checked ? [...value.events, ev] : value.events.filter((x) => x !== ev) })}
                />
              ))}
              {eventTypes === undefined ? <span className="text-xs text-fg-faint col-span-2">{t("common.loading")}</span> : null}
            </div>
          ) : null}
        </div>
      </Field>
    </div>
  );
}

export function AppWebhooksTab({ appId }: { appId: string }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const toast = useToast();
  const now = useNow();
  const hooks = useAppWebhooksQuery(appId);
  const eventTypes = useEventTypesQuery(appId);
  const create = useCreateWebhookMutation(appId);
  const update = useUpdateWebhookMutation(appId);
  const del = useDeleteWebhookMutation(appId);
  const rotate = useRotateWebhookSecretMutation(appId);
  const test = useTestWebhookMutation(appId);
  const resync = useResyncWebhookMutation(appId);
  const canWrite = canApp("webhooks:write");

  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<T.Webhook | null>(null);
  const [form, setForm] = useState<WebhookForm>(EMPTY_FORM);
  const [deleting, setDeleting] = useState<T.Webhook | null>(null);
  const [rotating, setRotating] = useState<T.Webhook | null>(null);
  const [secret, setSecret] = useState<T.Webhook | null>(null);
  const [deliveriesOf, setDeliveriesOf] = useState<T.Webhook | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  const openEdit = (w: T.Webhook) => {
    setForm({ url: w.url, description: w.description ?? "", allEvents: w.events.includes("*"), events: w.events.filter((e) => e !== "*") });
    setEditing(w);
  };

  const validate = (): T.CreateWebhookRequest => {
    const url = form.url.trim();
    if (!/^https?:\/\//i.test(url)) throw new Error(t("webhooks.url.invalid"));
    const events = form.allEvents ? ["*"] : form.events;
    if (events.length === 0) throw new Error(t("webhooks.events.required"));
    return { url, events, description: form.description.trim() || null };
  };

  const run = useCallback(
    async (w: T.Webhook, fn: () => Promise<void>) => {
      setBusyId(w.id);
      try {
        await fn();
      } catch (e) {
        toast.error(t("common.error"), errorMessage(e, locale));
      } finally {
        setBusyId(null);
      }
    },
    [t, locale, toast],
  );

  const columns = useMemo<Column<T.Webhook>[]>(
    () => [
      {
        key: "url",
        header: t("webhooks.url"),
        sort: (w) => w.url,
        cell: (w) => (
          <div className="flex flex-col min-w-0 max-w-md">
            <Mono className="truncate" title={w.url}>
              {w.url}
            </Mono>
            {w.description ? <span className="text-xs text-fg-muted truncate">{w.description}</span> : <IdChip id={w.id} />}
          </div>
        ),
      },
      {
        key: "events",
        header: t("webhooks.events"),
        cell: (w) =>
          w.events.includes("*") ? (
            <Badge tone="accent">{t("webhooks.events.all")}</Badge>
          ) : (
            <Tip content={<span className="font-mono text-[11px] whitespace-pre-wrap">{w.events.join("\n")}</span>}>
              <span className="text-fg-muted tabular">{t("common.count.items", { n: w.events.length })}</span>
            </Tip>
          ),
      },
      {
        key: "enabled",
        header: t("common.status"),
        sort: (w) => (w.enabled ? 1 : 0),
        cell: (w) => (
          <div className="flex items-center gap-2">
            <Switch
              checked={w.enabled}
              disabled={!canWrite || busyId === w.id}
              onCheckedChange={(v) =>
                void run(w, async () => {
                  await update.mutateAsync({ webhookId: w.id, body: { enabled: v } });
                  toast.ok(t("webhooks.updated"));
                })
              }
              label={<span className={w.enabled ? "" : "text-fg-muted"}>{w.enabled ? t("common.enabled") : t("common.disabled")}</span>}
            />
          </div>
        ),
      },
      {
        key: "failures",
        header: t("webhooks.failures"),
        align: "right",
        sort: (w) => w.consecutive_failures,
        cell: (w) => <span className={w.consecutive_failures > 0 ? "tabular text-danger font-medium" : "tabular text-fg-muted"}>{w.consecutive_failures}</span>,
      },
      {
        key: "last",
        header: t("webhooks.lastDelivery"),
        sort: (w) => (w.last_delivery_at ? Date.parse(w.last_delivery_at) : null),
        cell: (w) => (
          <div className="flex items-center gap-2">
            {w.last_status != null ? <Badge tone={httpTone(w.last_status)}>HTTP {w.last_status}</Badge> : null}
            {w.last_delivery_at ? (
              <Tip content={w.last_error ?? fmtDateTime(locale, w.last_delivery_at)}>
                <span className="text-fg-muted whitespace-nowrap">{fmtRelative(locale, w.last_delivery_at, now)}</span>
              </Tip>
            ) : (
              <span className="text-fg-faint">{t("common.never")}</span>
            )}
          </div>
        ),
      },
      {
        key: "actions",
        header: "",
        align: "right",
        cell: (w) => (
          <div className="flex items-center justify-end gap-1">
            <Button size="xs" variant="ghost" onClick={() => setDeliveriesOf(w)}>
              {t("webhooks.deliveries")}
            </Button>
            {canWrite ? (
              <Menu
                label={t("common.actions")}
                trigger={
                  <Button size="icon" variant="ghost" aria-label={t("common.actions")} loading={busyId === w.id}>
                    <MoreHorizontal className="size-4" />
                  </Button>
                }
                items={[
                  { label: t("common.edit"), onSelect: () => openEdit(w) },
                  {
                    label: t("webhooks.test"),
                    onSelect: () =>
                      void run(w, async () => {
                        await test.mutateAsync({ webhookId: w.id });
                        toast.ok(t("webhooks.test.sent"));
                        setDeliveriesOf(w);
                      }),
                  },
                  {
                    label: t("webhooks.resync"),
                    onSelect: () =>
                      void run(w, async () => {
                        await resync.mutateAsync({ webhookId: w.id });
                        toast.ok(t("webhooks.resynced"));
                      }),
                  },
                  { label: t("webhooks.rotateSecret"), onSelect: () => setRotating(w), separatorBefore: true },
                  { label: t("common.delete"), onSelect: () => setDeleting(w), danger: true, separatorBefore: true },
                ]}
              />
            ) : null}
          </div>
        ),
      },
    ],
    [t, locale, now, canWrite, busyId, update, test, resync, toast, run],
  );

  if (!canApp("webhooks:read")) {
    return <EmptyState compact title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "webhooks:read" })} />;
  }

  const list = hooks.data?.webhooks;
  const disabledCount = list?.filter((w) => !w.enabled).length ?? 0;

  return (
    <div className="flex flex-col">
      <Toolbar
        end={
          canWrite ? (
            <Button
              size="sm"
              variant="primary"
              onClick={() => {
                setForm(EMPTY_FORM);
                setCreating(true);
              }}
            >
              <Plus className="size-4" />
              {t("webhooks.new")}
            </Button>
          ) : null
        }
      >
        <span className="text-xs text-fg-muted">{t("webhooks.desc")}</span>
        {eventTypes.data ? (
          <span className="text-xs text-fg-faint">
            · <Mono>{eventTypes.data.signature_header}</Mono> ({eventTypes.data.signature_scheme})
          </span>
        ) : null}
      </Toolbar>
      {disabledCount > 0 ? (
        <div className="px-3 pt-3">
          <Callout tone="warn">{t("webhooks.disabled.notice")}</Callout>
        </div>
      ) : null}
      <DataTable
        rows={list}
        columns={columns}
        rowKey={(w) => w.id}
        loading={hooks.isPending}
        dense
        error={hooks.isError ? <QueryError error={hooks.error} onRetry={() => void hooks.refetch()} compact /> : undefined}
        empty={<EmptyState compact icon={<WebhookIcon className="size-6" strokeWidth={1.5} />} title={t("webhooks.empty")} description={t("webhooks.empty.desc")} />}
      />

      <FormDialog
        open={creating}
        onOpenChange={setCreating}
        title={t("webhooks.new")}
        submitLabel={t("common.create")}
        onSubmit={async () => {
          const body = validate();
          const w = await create.mutateAsync(body);
          toast.ok(t("webhooks.created"));
          setSecret(w);
        }}
      >
        <WebhookFormFields value={form} onChange={setForm} eventTypes={eventTypes.data?.webhook} />
      </FormDialog>

      <FormDialog
        open={editing !== null}
        onOpenChange={(o) => {
          if (!o) setEditing(null);
        }}
        title={t("common.edit")}
        submitLabel={t("common.save")}
        onSubmit={async () => {
          if (!editing) return;
          const body = validate();
          await update.mutateAsync({ webhookId: editing.id, body });
          toast.ok(t("webhooks.updated"));
        }}
      >
        <WebhookFormFields value={form} onChange={setForm} eventTypes={eventTypes.data?.webhook} />
      </FormDialog>

      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => {
          if (!o) setDeleting(null);
        }}
        title={t("common.delete")}
        description={deleting ? t("webhooks.delete.desc", { url: deleting.url }) : undefined}
        confirmLabel={t("common.delete")}
        variant="danger"
        onConfirm={async () => {
          if (!deleting) return;
          await del.mutateAsync({ webhookId: deleting.id });
          toast.ok(t("webhooks.deleted"));
        }}
      />

      <ConfirmDialog
        open={rotating !== null}
        onOpenChange={(o) => {
          if (!o) setRotating(null);
        }}
        title={t("webhooks.rotateSecret")}
        description={t("webhooks.rotateSecret.desc")}
        confirmLabel={t("webhooks.rotateSecret")}
        onConfirm={async () => {
          if (!rotating) return;
          const w = await rotate.mutateAsync({ webhookId: rotating.id });
          setSecret(w);
        }}
      />

      <SecretReveal
        secret={secret?.secret ?? null}
        title={t("webhooks.secret")}
        notice={t("webhooks.secret.notice")}
        meta={secret ? [{ k: t("webhooks.url"), v: <Mono>{secret.url}</Mono> }, { k: t("common.id"), v: <IdChip id={secret.id} short={36} /> }] : undefined}
        onClose={() => setSecret(null)}
      />

      <DeliveriesDialog appId={appId} webhook={deliveriesOf} onClose={() => setDeliveriesOf(null)} />
    </div>
  );
}
