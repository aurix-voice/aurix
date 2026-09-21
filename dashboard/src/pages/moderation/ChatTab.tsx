import { ChevronDown, ChevronUp, MessageSquare, RefreshCw, Search, Send, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useChannelsQuery, useChatPageQuery, useDeleteMessageMutation, useSendDirectMessageMutation, useSendSystemMessageMutation, type ChatTarget } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { cn } from "@/lib/cn";
import { fmtDateTime, fmtTime, shortId } from "@/lib/format";
import { Button } from "@/ui/Button";
import { ConfirmDialog } from "@/ui/Dialog";
import { Input, NativeSelect, Textarea } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { QueryError, Toolbar } from "@/ui/Page";
import { Badge, Card, CopyButton, EmptyState, Mono, Skeleton, Tip } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { UserRef, useModSearch } from "./shared";
import { SYSTEM_USER } from "./model";
import { isUuid, UserPicker } from "./UserPicker";

const LIMIT = 50;
type Kind = "channel" | "user";
/** Nil UUID: sender of REST system messages and `deleted_by` of API deletions. */

/** Channel / user chat history with full-text search, cursor paging, delete and system messages. */
export function ChatTab() {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const toast = useToast();
  const { channel: channelFromUrl, user: userFromUrl, go } = useModSearch();

  const [kind, setKind] = useState<Kind>(userFromUrl && !channelFromUrl ? "user" : "channel");
  const [channelId, setChannelId] = useState(channelFromUrl ?? "");
  const [userId, setUserId] = useState(userFromUrl ?? "");
  const [peer, setPeer] = useState("");
  const [from, setFrom] = useState("");
  const [q, setQ] = useState("");
  const [cursor, setCursor] = useState<{ before?: string; after?: string }>({});
  const [deleting, setDeleting] = useState<T.ChatMessage | null>(null);
  const [compose, setCompose] = useState("");
  const [composeName, setComposeName] = useState("");

  const channels = useChannelsQuery({ page: 1, per_page: 200, active_only: false });
  const channelOptions = useMemo(() => (channels.data?.data ?? []).slice().sort((a, b) => a.name.localeCompare(b.name)), [channels.data]);

  const target: ChatTarget | null = useMemo(() => {
    if (kind === "channel") return isUuid(channelId) ? { kind: "channel", id: channelId.trim() } : null;
    if (!isUuid(userId)) return null;
    return { kind: "user", id: userId.trim(), peer: isUuid(peer) ? peer.trim() : undefined };
  }, [kind, channelId, userId, peer]);

  const searching = q.trim() !== "";
  const page = useChatPageQuery({ target, q, before: cursor.before, after: cursor.after, fromUserId: isUuid(from) ? from.trim() : undefined, limit: LIMIT });
  const del = useDeleteMessageMutation();
  const sendChannel = useSendSystemMessageMutation();
  const sendDirect = useSendDirectMessageMutation();
  const sending = sendChannel.isPending || sendDirect.isPending;

  const setTarget = (patch: { kind?: Kind; channelId?: string; userId?: string; peer?: string }) => {
    if (patch.kind !== undefined) setKind(patch.kind);
    if (patch.channelId !== undefined) setChannelId(patch.channelId);
    if (patch.userId !== undefined) setUserId(patch.userId);
    if (patch.peer !== undefined) setPeer(patch.peer);
    setCursor({});
    const nextKind = patch.kind ?? kind;
    const nextChannel = patch.channelId ?? channelId;
    const nextUser = patch.userId ?? userId;
    go({
      channel: nextKind === "channel" && isUuid(nextChannel) ? nextChannel.trim() : null,
      user: nextKind === "user" && isUuid(nextUser) ? nextUser.trim() : null,
    });
  };

  const messages = page.data?.messages ?? [];
  const canSend = canApp("chat:write") && target !== null && !searching;

  const send = async () => {
    if (!target || !compose.trim()) return;
    const body: T.SystemMessageRequest = { text: compose.trim(), display_name: composeName.trim() || null };
    try {
      if (target.kind === "channel") await sendChannel.mutateAsync({ channelId: target.id, body });
      else await sendDirect.mutateAsync({ userId: target.id, body });
      setCompose("");
      setCursor({});
      toast.ok(t("moderation.chat.sent"));
    } catch (e) {
      toast.error(errorMessage(e, locale));
    }
  };

  return (
    <div className="flex flex-col gap-3">
      <Toolbar
        end={
          <Button variant="ghost" size="icon" onClick={() => void page.refetch()} aria-label={t("common.refresh")} disabled={!target || page.isFetching}>
            <RefreshCw className={page.isFetching ? "size-4 animate-spin" : "size-4"} />
          </Button>
        }
      >
        <Segmented<Kind>
          size="sm"
          value={kind}
          onChange={(v) => setTarget({ kind: v })}
          options={[
            { value: "channel", label: t("moderation.chat.channel") },
            { value: "user", label: t("moderation.chat.user") },
          ]}
        />
        {kind === "channel" ? (
          <>
            <NativeSelect value={channelOptions.some((c) => c.id === channelId) ? channelId : ""} onChange={(e) => setTarget({ channelId: e.target.value })} className="h-8 w-56">
              <option value="">{t("moderation.chat.pickChannel")}</option>
              {channelOptions.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </NativeSelect>
            <Input value={channelId} onChange={(e) => setTarget({ channelId: e.target.value })} placeholder={t("moderation.chat.channelId")} className="w-72 font-mono text-[12.5px]" spellCheck={false} />
          </>
        ) : (
          <>
            <div className="w-72">
              <UserPicker value={userId} onChange={(v) => setTarget({ userId: v })} placeholder={t("moderation.chat.pickUser")} />
            </div>
            <div className="w-64">
              <UserPicker value={peer} onChange={(v) => setTarget({ peer: v })} placeholder={t("moderation.chat.peer")} exclude={userId.trim() || undefined} />
            </div>
          </>
        )}
      </Toolbar>

      <Toolbar>
        <div className="relative w-96">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-fg-faint" />
          <Input
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setCursor({});
            }}
            placeholder={t("moderation.chat.search")}
            className="pl-8"
            title={t("moderation.chat.searchHint")}
          />
        </div>
        {searching ? (
          <>
            <div className="w-64">
              <UserPicker
                value={from}
                onChange={(v) => {
                  setFrom(v);
                  setCursor({});
                }}
                placeholder={t("moderation.chat.from")}
              />
            </div>
            <Badge tone="accent">{t("moderation.chat.searchMode")}</Badge>
          </>
        ) : null}
      </Toolbar>

      <Card className="flex flex-col">
        {!target ? (
          <EmptyState compact icon={<MessageSquare className="size-5" />} title={kind === "channel" ? t("moderation.chat.pickChannel") : t("moderation.chat.pickUser")} description={t("moderation.chat.target.desc")} />
        ) : page.isError ? (
          <div className="p-4">
            <QueryError error={page.error} onRetry={() => void page.refetch()} compact />
          </div>
        ) : page.isPending ? (
          <div className="flex flex-col gap-2 p-4">
            <Skeleton className="h-6" />
            <Skeleton className="h-6" />
            <Skeleton className="h-6" />
          </div>
        ) : (
          <>
            <div className="flex items-center justify-between border-b border-border px-3 py-1.5">
              <Button size="xs" variant="ghost" disabled={!page.data?.next_before} onClick={() => setCursor({ before: page.data?.next_before })}>
                <ChevronUp className="size-3.5" /> {t("moderation.chat.older")}
              </Button>
              <span className="text-[12px] text-fg-muted">
                {t("common.count.items", { n: messages.length })}
                {cursor.before || cursor.after ? (
                  <button type="button" className="ml-2 underline-offset-2 hover:underline" onClick={() => setCursor({})}>
                    {t("moderation.chat.latest")}
                  </button>
                ) : null}
              </span>
              <Button size="xs" variant="ghost" disabled={searching || !page.data?.next_after} onClick={() => setCursor({ after: page.data?.next_after })}>
                {t("moderation.chat.newer")} <ChevronDown className="size-3.5" />
              </Button>
            </div>
            {messages.length === 0 ? (
              <EmptyState compact icon={<MessageSquare className="size-5" />} title={searching ? t("common.noResults") : t("moderation.chat.empty")} />
            ) : (
              <ul className="flex flex-col-reverse divide-y divide-y-reverse divide-border">
                {messages.map((m) => (
                  <MessageRow key={m.id} m={m} showTarget={kind === "user"} canDelete={canApp("chat:write") && !m.deleted_at} onDelete={() => setDeleting(m)} />
                ))}
              </ul>
            )}
          </>
        )}
        {canSend ? (
          <form
            className="flex flex-col gap-2 border-t border-border p-3"
            onSubmit={(e) => {
              e.preventDefault();
              void send();
            }}
          >
            <div className="flex items-start gap-2">
              <Textarea
                value={compose}
                onChange={(e) => setCompose(e.target.value)}
                rows={2}
                maxLength={4000}
                placeholder={kind === "channel" ? t("moderation.chat.send.placeholder") : t("moderation.chat.send.placeholderUser")}
                className="min-h-0 flex-1"
                onKeyDown={(e) => {
                  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                    e.preventDefault();
                    void send();
                  }
                }}
              />
              <div className="flex w-44 flex-col gap-2">
                <Input value={composeName} onChange={(e) => setComposeName(e.target.value)} placeholder={t("moderation.chat.send.name")} maxLength={64} />
                <Button type="submit" size="sm" loading={sending} disabled={!compose.trim()}>
                  <Send className="size-3.5" /> {t("moderation.chat.send")}
                </Button>
              </div>
            </div>
          </form>
        ) : null}
      </Card>

      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => !o && setDeleting(null)}
        title={t("moderation.chat.delete")}
        description={t("moderation.chat.delete.desc")}
        confirmLabel={t("common.delete")}
        variant="danger"
        onConfirm={async () => {
          if (!deleting) return;
          try {
            await del.mutateAsync({ messageId: deleting.id });
            toast.ok(t("moderation.chat.deletedDone"));
          } catch (e) {
            throw new Error(errorMessage(e, locale), { cause: e });
          }
        }}
      >
        {deleting ? <blockquote className="rounded-lg bg-surface-2 px-2.5 py-1.5 text-[12.5px] text-fg-muted">{deleting.text}</blockquote> : null}
      </ConfirmDialog>
    </div>
  );
}

function MessageRow({ m, showTarget, canDelete, onDelete }: { m: T.ChatMessage; showTarget: boolean; canDelete: boolean; onDelete: () => void }) {
  const { t, locale } = useI18n();
  const deleted = !!m.deleted_at;
  const system = m.from_user_id === SYSTEM_USER;
  return (
    <li className={cn("group flex gap-3 px-3 py-2 text-[13px]", deleted && "opacity-70")}>
      <Tip content={fmtDateTime(locale, m.sent_at)}>
        <span className="w-16 shrink-0 pt-px font-mono text-[11px] text-fg-faint">{fmtTime(locale, m.sent_at)}</span>
      </Tip>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
          {system ? <span className="font-medium">{m.display_name || "Server"}</span> : <UserRef id={m.from_user_id} label={<span className="font-medium">{m.display_name || shortId(m.from_user_id)}</span>} />}
          {showTarget && m.to_user_id ? (
            <span className="text-fg-faint">
              → <UserRef id={m.to_user_id} />
            </span>
          ) : null}
          {showTarget && m.channel_id ? (
            <span className="text-fg-faint">
              # <Mono>{shortId(m.channel_id)}</Mono>
            </span>
          ) : null}
          {system ? <Badge>{t("moderation.chat.system")}</Badge> : null}
          {m.offline ? <Badge tone="neutral">{t("moderation.chat.offline")}</Badge> : null}
          {m.edited_at ? (
            <span className="text-[11px] text-fg-faint" title={fmtDateTime(locale, m.edited_at)}>
              {t("moderation.chat.edited")}
            </span>
          ) : null}
          {deleted ? (
            <Badge tone="danger">
              {t("moderation.chat.deleted")}
              {m.deleted_by ? ` · ${m.deleted_by === SYSTEM_USER ? t("moderation.chat.system") : shortId(m.deleted_by)}` : ""}
            </Badge>
          ) : null}
          <span className="ml-auto flex items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100">
            <CopyButton value={m.id} />
            {canDelete ? (
              <Button size="xs" variant="ghost" className="text-danger" onClick={onDelete} aria-label={t("moderation.chat.delete")}>
                <Trash2 className="size-3.5" />
              </Button>
            ) : null}
          </span>
        </div>
        <p className={cn("mt-0.5 whitespace-pre-wrap break-words", deleted && "italic text-fg-muted")}>{deleted ? t("moderation.chat.deletedText") : m.text}</p>
        {m.reactions?.length ? (
          <div className="mt-1 flex flex-wrap gap-1">
            {m.reactions.map((r) => (
              <Tip key={r.reaction} content={r.user_ids?.length ? r.user_ids.map((u) => shortId(u)).join(", ") : t("moderation.chat.reactions")}>
                <span className="inline-flex items-center gap-1 rounded-full border border-border bg-surface-2 px-1.5 py-px text-[11.5px]">
                  {r.reaction} <span className="tabular-nums text-fg-muted">{r.count}</span>
                </span>
              </Tip>
            ))}
          </div>
        ) : null}
      </div>
    </li>
  );
}
