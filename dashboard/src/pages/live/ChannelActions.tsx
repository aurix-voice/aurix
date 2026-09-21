import { Megaphone, MessageSquare, MicOff, Mic, Trash2, UserX } from "lucide-react";
import { useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useAnnounceMutation, useDeleteChannelMutation, useKickAllMutation, useMuteAllMutation, useSendSystemMessageMutation } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { ConfirmDialog, FormDialog } from "@/ui/Dialog";
import { Input, Textarea } from "@/ui/Input";
import { type MenuItem } from "@/ui/Menu";
import { Callout, Field } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

export type ChannelAction = "muteAll" | "unmuteAll" | "kickAll" | "delete" | "announce" | "message";

/** Menu items for a channel, gated by the delegated tenant permissions. */
export function useChannelMenuItems(channel: Pick<T.Channel, "id" | "name" | "ad_hoc" | "active_participants">, open: (a: ChannelAction) => void): MenuItem[] {
  const { t } = useI18n();
  const { canApp } = useAuth();
  const mod = canApp("moderation:write");
  const empty = !channel.active_participants;
  return [
    { label: t("live.muteAll"), icon: <MicOff />, onSelect: () => open("muteAll"), hidden: !mod, disabled: empty },
    { label: t("live.unmuteAll"), icon: <Mic />, onSelect: () => open("unmuteAll"), hidden: !mod, disabled: empty },
    { label: t("live.kickAll"), icon: <UserX />, onSelect: () => open("kickAll"), hidden: !mod, disabled: empty, danger: true },
    { label: t("live.systemMessage"), icon: <MessageSquare />, onSelect: () => open("message"), hidden: !canApp("chat:write"), separatorBefore: true },
    { label: t("live.announce"), icon: <Megaphone />, onSelect: () => open("announce"), hidden: !canApp("tts:write") },
    { label: t("live.deleteChannel"), icon: <Trash2 />, onSelect: () => open("delete"), hidden: !canApp("channels:write"), danger: true, separatorBefore: true },
  ];
}

/** The dialogs behind `useChannelMenuItems`; render once per page and drive with `action`. */
export function ChannelActionDialogs({
  channel,
  action,
  onClose,
  onDeleted,
}: {
  channel: Pick<T.Channel, "id" | "name"> | null;
  action: ChannelAction | null;
  onClose: () => void;
  onDeleted?: () => void;
}) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const muteAll = useMuteAllMutation();
  const kickAll = useKickAllMutation();
  const del = useDeleteChannelMutation();
  const announce = useAnnounceMutation();
  const message = useSendSystemMessageMutation();
  const [reason, setReason] = useState("");
  const [text, setText] = useState("");
  const [voice, setVoice] = useState("");
  const [sender, setSender] = useState("");

  if (!channel) return null;
  const name = channel.name;
  const close = () => {
    setReason("");
    setText("");
    onClose();
  };
  const reportBulk = (r: T.BulkModerationResult) => {
    if (r.failed.length) toast.error(t("live.bulk.partial", { ok: r.affected.length, failed: r.failed.length }));
    else toast.ok(t("live.bulk.done", { n: r.affected.length }));
  };

  return (
    <>
      <ConfirmDialog
        open={action === "muteAll" || action === "unmuteAll"}
        onOpenChange={(o) => !o && close()}
        title={action === "muteAll" ? t("live.muteAll") : t("live.unmuteAll")}
        description={action === "muteAll" ? t("live.muteAll.desc", { channel: name }) : t("live.unmuteAll.desc", { channel: name })}
        confirmLabel={action === "muteAll" ? t("live.muteAll") : t("live.unmuteAll")}
        onConfirm={async () => reportBulk(await muteAll.mutateAsync({ channel_id: channel.id, muted: action === "muteAll" }))}
      />
      <ConfirmDialog
        open={action === "kickAll"}
        onOpenChange={(o) => !o && close()}
        title={t("live.kickAll")}
        description={t("live.kickAll.desc", { channel: name })}
        confirmLabel={t("live.kickAll")}
        variant="danger"
        disabled={!reason.trim()}
        onConfirm={async () => reportBulk(await kickAll.mutateAsync({ channel_id: channel.id, reason: reason.trim() }))}
      >
        <Field label={t("common.reason")} required>
          <Input value={reason} onChange={(e) => setReason(e.target.value)} autoFocus />
        </Field>
      </ConfirmDialog>
      <ConfirmDialog
        open={action === "delete"}
        onOpenChange={(o) => !o && close()}
        title={t("live.deleteChannel")}
        description={t("live.deleteChannel.desc", { name })}
        confirmLabel={t("common.delete")}
        variant="danger"
        onConfirm={async () => {
          await del.mutateAsync({ channelId: channel.id });
          toast.ok(t("live.channelDeleted"));
          onDeleted?.();
        }}
      />
      <FormDialog
        open={action === "message"}
        onOpenChange={(o) => !o && close()}
        title={t("live.systemMessage")}
        description={t("live.systemMessage.desc")}
        submitLabel={t("live.send")}
        disabled={!text.trim()}
        onSubmit={async () => {
          try {
            await message.mutateAsync({ channelId: channel.id, body: { text: text.trim(), display_name: sender.trim() || null } });
            toast.ok(t("live.messageSent"));
            close();
          } catch (e) {
            throw new Error(errorMessage(e, locale), { cause: e });
          }
        }}
      >
        <Field label={t("live.announce.text")} required>
          <Textarea value={text} onChange={(e) => setText(e.target.value)} maxLength={2000} autoFocus />
        </Field>
        <Field label={t("live.systemMessage.sender")} hint={t("live.systemMessage.sender.hint")}>
          <Input value={sender} onChange={(e) => setSender(e.target.value)} placeholder="System" />
        </Field>
      </FormDialog>
      <FormDialog
        open={action === "announce"}
        onOpenChange={(o) => !o && close()}
        title={t("live.announce")}
        description={t("live.announce.desc")}
        submitLabel={t("live.announce.button")}
        disabled={!text.trim()}
        onSubmit={async () => {
          try {
            await announce.mutateAsync({ channelId: channel.id, body: { text: text.trim(), voice: voice.trim() || null } });
            toast.ok(t("live.announced"));
            close();
          } catch (e) {
            throw new Error(errorMessage(e, locale), { cause: e });
          }
        }}
      >
        <Field label={t("live.announce.text")} required>
          <Textarea value={text} onChange={(e) => setText(e.target.value)} maxLength={1000} autoFocus />
        </Field>
        <Field label={t("live.announce.voice")} hint={t("live.announce.voice.hint")}>
          <Input value={voice} onChange={(e) => setVoice(e.target.value)} />
        </Field>
        <Callout tone="neutral">{t("live.tts.hint")}</Callout>
      </FormDialog>
    </>
  );
}
