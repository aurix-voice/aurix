import { KeyRound, Plus } from "lucide-react";
import { useMemo, useState } from "react";

import type { T } from "@/api/client";
import { useAppKeysQuery, useCreateApiKeyMutation, useRevokeApiKeyMutation, useUpdateApiKeyMutation } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { delegatedAppPermissions, KNOWN_KEY_PERMISSIONS } from "@/auth/store";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtNumber, fmtRelative } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { Button } from "@/ui/Button";
import { ConfirmDialog, FormDialog } from "@/ui/Dialog";
import { Checkbox, Input } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Callout, EmptyState, Field, IdChip, Mono, Tip } from "@/ui/Primitives";
import { DataTable, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { SecretReveal } from "./SecretReveal";

type PermMode = "all" | "custom";

function keyState(k: T.ApiKey, now: number): "active" | "revoked" | "expired" {
  if (k.revoked_at || !k.active) return "revoked";
  if (k.expires_at && Date.parse(k.expires_at) < now) return "expired";
  return "active";
}

export function AppKeysTab({ appId }: { appId: string }) {
  const { t, locale } = useI18n();
  const { admin, canApp } = useAuth();
  const toast = useToast();
  const now = useNow(30_000);
  const keys = useAppKeysQuery(appId);
  const create = useCreateApiKeyMutation(appId);
  const update = useUpdateApiKeyMutation(appId);
  const revoke = useRevokeApiKeyMutation(appId);

  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [permMode, setPermMode] = useState<PermMode>("all");
  const [perms, setPerms] = useState<string[]>([]);
  const [rateLimit, setRateLimit] = useState("");
  const [expires, setExpires] = useState("");
  const [createdKey, setCreatedKey] = useState<T.CreatedApiKey | null>(null);
  const [revoking, setRevoking] = useState<T.ApiKey | null>(null);
  const [editing, setEditing] = useState<T.ApiKey | null>(null);
  const [editRate, setEditRate] = useState("");

  const delegated = delegatedAppPermissions(admin);
  const grantable = delegated === null ? KNOWN_KEY_PERMISSIONS : delegated;
  const canGrantAll = delegated === null;

  const columns = useMemo<Column<T.ApiKey>[]>(
    () => [
      {
        key: "name",
        header: t("common.name"),
        sort: (k) => k.name,
        cell: (k) => (
          <div className="flex flex-col min-w-0">
            <span className="font-medium truncate">{k.name}</span>
            <IdChip id={k.id} />
          </div>
        ),
      },
      { key: "prefix", header: t("keys.prefix"), cell: (k) => <Mono>{k.key_prefix}…</Mono> },
      {
        key: "perms",
        header: t("keys.permissions"),
        cell: (k) =>
          k.permissions.includes("*") ? (
            <Badge tone="accent">{t("keys.permissions.all")}</Badge>
          ) : (
            <div className="flex flex-wrap gap-1 max-w-sm">
              {k.permissions.map((p) => (
                <Badge key={p}>
                  <span className="font-mono text-[11px]">{p}</span>
                </Badge>
              ))}
            </div>
          ),
      },
      {
        key: "rate",
        header: t("keys.rateLimit"),
        align: "right",
        sort: (k) => k.rate_limit ?? 0,
        cell: (k) => <span className="tabular">{k.rate_limit === 0 ? t("common.unlimited") : k.rate_limit === undefined ? "—" : `${fmtNumber(locale, k.rate_limit)}/min`}</span>,
      },
      {
        key: "status",
        header: t("common.status"),
        sort: (k) => keyState(k, now),
        cell: (k) => {
          const s = keyState(k, now);
          return (
            <Badge tone={s === "active" ? "ok" : s === "expired" ? "warn" : "neutral"} dot>
              {s === "active" ? t("common.active") : s === "expired" ? t("keys.expired") : t("keys.revoked.state")}
            </Badge>
          );
        },
      },
      {
        key: "lastUsed",
        header: t("keys.lastUsed"),
        sort: (k) => (k.last_used_at ? Date.parse(k.last_used_at) : null),
        cell: (k) =>
          k.last_used_at ? (
            <Tip content={fmtDateTime(locale, k.last_used_at)}>
              <span className="text-fg-muted">{fmtRelative(locale, k.last_used_at, now)}</span>
            </Tip>
          ) : (
            <span className="text-fg-faint">{t("common.never")}</span>
          ),
      },
      {
        key: "expires",
        header: t("common.expires"),
        sort: (k) => (k.expires_at ? Date.parse(k.expires_at) : null),
        cell: (k) => <span className="text-fg-muted whitespace-nowrap">{k.expires_at ? fmtDateTime(locale, k.expires_at) : t("keys.noExpiry")}</span>,
      },
      {
        key: "created",
        header: t("common.created"),
        sort: (k) => Date.parse(k.created_at),
        cell: (k) => <span className="text-fg-muted whitespace-nowrap">{fmtDateTime(locale, k.created_at)}</span>,
      },
      {
        key: "actions",
        header: "",
        align: "right",
        cell: (k) =>
          keyState(k, now) === "revoked" ? null : (
            <div className="flex items-center justify-end gap-1">
              <Button
                size="xs"
                variant="ghost"
                onClick={() => {
                  setEditing(k);
                  setEditRate(String(k.rate_limit ?? ""));
                }}
              >
                {t("keys.rateLimit")}
              </Button>
              <Button size="xs" variant="ghost" className="text-danger" onClick={() => setRevoking(k)}>
                {t("keys.revoke")}
              </Button>
            </div>
          ),
      },
    ],
    [t, locale, now],
  );

  if (!canApp("keys:manage")) {
    return <EmptyState compact title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "keys:manage" })} />;
  }

  const submitCreate = async () => {
    const n = name.trim();
    if (!n) throw new Error(t("common.required"));
    const rl = rateLimit.trim();
    const ex = expires.trim();
    if (rl && !/^\d+$/.test(rl)) throw new Error(t("keys.rateLimit.invalid"));
    if (ex && (!/^\d+$/.test(ex) || Number(ex) < 1)) throw new Error(t("keys.expiresIn.invalid"));
    const permissions = permMode === "all" ? ["*"] : perms;
    if (permissions.length === 0) throw new Error(t("keys.permissions.required"));
    const res = await create.mutateAsync({
      name: n,
      permissions,
      rate_limit: rl ? Number(rl) : null,
      expires_in_days: ex ? Number(ex) : null,
    });
    setName("");
    setPerms([]);
    setRateLimit("");
    setExpires("");
    setPermMode(canGrantAll ? "all" : "custom");
    setCreatedKey(res);
    toast.ok(t("keys.created"));
  };

  const submitRate = async () => {
    if (!editing) return;
    const rl = editRate.trim();
    if (!/^\d+$/.test(rl)) throw new Error(t("keys.rateLimit.invalid"));
    await update.mutateAsync({ keyId: editing.id, body: { rate_limit: Number(rl) } });
    toast.ok(t("keys.updated"));
  };

  return (
    <div className="flex flex-col">
      <Toolbar
        end={
          <Button
            size="sm"
            variant="primary"
            onClick={() => {
              setPermMode(canGrantAll ? "all" : "custom");
              setCreating(true);
            }}
          >
            <Plus className="size-4" />
            {t("keys.new")}
          </Button>
        }
      >
        <span className="text-xs text-fg-muted">{t("keys.desc")}</span>
      </Toolbar>
      <DataTable
        rows={keys.data}
        columns={columns}
        rowKey={(k) => k.id}
        loading={keys.isPending}
        error={keys.isError ? <QueryError error={keys.error} onRetry={() => void keys.refetch()} compact /> : undefined}
        dense
        empty={<EmptyState compact icon={<KeyRound className="size-6" strokeWidth={1.5} />} title={t("keys.empty")} description={t("keys.empty.desc")} />}
      />

      <FormDialog open={creating} onOpenChange={setCreating} title={t("keys.new")} submitLabel={t("common.create")} onSubmit={submitCreate}>
        <div className="flex flex-col gap-3">
          <Field label={t("common.name")} required htmlFor="key-name">
            <Input id="key-name" value={name} onChange={(e) => setName(e.target.value)} autoFocus required maxLength={128} />
          </Field>
          <Field label={t("keys.permissions")}>
            <div className="flex flex-col gap-2">
              <Segmented
                size="sm"
                value={permMode}
                onChange={setPermMode}
                options={[
                  ...(canGrantAll ? [{ value: "all" as const, label: t("keys.permissions.all") }] : []),
                  { value: "custom" as const, label: t("keys.permissions.custom") },
                ]}
              />
              {permMode === "custom" ? (
                <div className="grid grid-cols-2 gap-1.5 rounded-md border border-border p-2.5 max-h-56 overflow-auto subtle-scroll">
                  {grantable.map((p) => (
                    <Checkbox
                      key={p}
                      label={p}
                      className="font-mono text-[12px]"
                      checked={perms.includes(p)}
                      onChange={(e) => setPerms((xs) => (e.target.checked ? [...xs, p] : xs.filter((x) => x !== p)))}
                    />
                  ))}
                </div>
              ) : null}
              {!canGrantAll ? <span className="text-xs text-fg-faint">{t("keys.permissions.delegated")}</span> : null}
            </div>
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label={t("keys.rateLimit")} hint={t("keys.rateLimit.hint")} htmlFor="key-rl">
              <Input id="key-rl" inputMode="numeric" value={rateLimit} onChange={(e) => setRateLimit(e.target.value)} placeholder="6000" />
            </Field>
            <Field label={t("keys.expiresIn")} hint={t("keys.noExpiry")} htmlFor="key-exp">
              <Input id="key-exp" inputMode="numeric" value={expires} onChange={(e) => setExpires(e.target.value)} placeholder="—" />
            </Field>
          </div>
        </div>
      </FormDialog>

      <FormDialog
        open={editing !== null}
        onOpenChange={(o) => {
          if (!o) setEditing(null);
        }}
        title={editing ? `${t("keys.rateLimit")} — ${editing.name}` : ""}
        submitLabel={t("common.save")}
        onSubmit={submitRate}
        size="sm"
      >
        <Field label={t("keys.rateLimit")} hint={t("keys.rateLimit.hint")} htmlFor="key-edit-rl">
          <Input id="key-edit-rl" inputMode="numeric" value={editRate} onChange={(e) => setEditRate(e.target.value)} autoFocus />
        </Field>
      </FormDialog>

      <ConfirmDialog
        open={revoking !== null}
        onOpenChange={(o) => {
          if (!o) setRevoking(null);
        }}
        title={t("keys.revoke")}
        description={revoking ? t("keys.revoke.desc", { name: revoking.name, prefix: revoking.key_prefix }) : undefined}
        confirmLabel={t("keys.revoke")}
        variant="danger"
        onConfirm={async () => {
          if (!revoking) return;
          await revoke.mutateAsync({ keyId: revoking.id });
          toast.ok(t("keys.revoked"));
        }}
      >
        {revoking?.permissions.includes("*") ? <Callout tone="warn">{t("keys.revoke.defaultWarning")}</Callout> : null}
      </ConfirmDialog>

      <SecretReveal
        secret={createdKey?.key ?? null}
        title={t("keys.created")}
        notice={t("keys.created.notice")}
        meta={
          createdKey
            ? [
                { k: t("common.name"), v: createdKey.name },
                { k: t("keys.permissions"), v: <span className="font-mono text-[12px]">{createdKey.permissions.join(", ")}</span> },
                { k: t("common.expires"), v: createdKey.expires_at ? fmtDateTime(locale, createdKey.expires_at) : t("keys.noExpiry") },
              ]
            : undefined
        }
        onClose={() => setCreatedKey(null)}
      />
    </div>
  );
}
