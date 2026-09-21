import { useMemo, useState } from "react";

import type { T } from "@/api/client";
import { useChannelsQuery, useMixdownMutation, useStartRecordingMutation } from "@/api/hooks";
import { useI18n } from "@/i18n";
import { fmtDateTime, fmtDuration, shortId } from "@/lib/format";
import { FormDialog } from "@/ui/Dialog";
import { Checkbox, Input, NativeSelect } from "@/ui/Input";
import { Callout, Field, Mono } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { isUuid, UserPicker } from "../moderation/UserPicker";
import { mixdownCandidates } from "./model";

function ChannelSelect({ value, onChange }: { value: string; onChange: (id: string) => void }) {
  const { t } = useI18n();
  const channels = useChannelsQuery({ page: 1, per_page: 200, active_only: false }, false);
  const options = useMemo(() => (channels.data?.data ?? []).slice().sort((a, b) => a.name.localeCompare(b.name)), [channels.data]);
  return (
    <div className="flex flex-col gap-1.5">
      <NativeSelect value={options.some((c) => c.id === value) ? value : ""} onChange={(e) => onChange(e.target.value)}>
        <option value="">{t("moderation.chat.pickChannel")}</option>
        {options.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name}
          </option>
        ))}
      </NativeSelect>
      <Input value={value} onChange={(e) => onChange(e.target.value)} placeholder={t("moderation.chat.channelId")} className="font-mono text-[12.5px]" spellCheck={false} />
    </div>
  );
}

/** Mounted only while open: form state starts fresh on every mount. */
export function StartRecordingDialog({
  open,
  onOpenChange,
  channelId,
  onStarted,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  channelId?: string | null;
  onStarted: (r: T.Recording) => void;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const start = useStartRecordingMutation();
  const [channel, setChannel] = useState(channelId ?? "");
  const [user, setUser] = useState("");
  const [session, setSession] = useState("");
  const valid = isUuid(channel) && isUuid(user) && (session.trim() === "" || isUuid(session));

  return (
    <FormDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t("recordings.start")}
      description={t("recordings.start.desc")}
      submitLabel={t("recordings.start")}
      disabled={!valid || start.isPending}
      onSubmit={async () => {
        const r = await start.mutateAsync({ channel_id: channel.trim(), user_id: user.trim(), session_id: session.trim() || null });
        toast.ok(t("recordings.start.done"));
        onStarted(r);
      }}
    >
      <Field label={t("common.channel")} required>
        <ChannelSelect value={channel} onChange={setChannel} />
      </Field>
      <Field label={t("common.user")} required hint={t("recordings.start.userHint")}>
        <UserPicker value={user} onChange={setUser} />
      </Field>
      <Field label={t("common.session")} hint={t("recordings.start.sessionHint")}>
        <Input value={session} onChange={(e) => setSession(e.target.value)} className="font-mono text-[12.5px]" spellCheck={false} placeholder={t("common.optional")} />
      </Field>
    </FormDialog>
  );
}

/** Mounted only while open: the selection is copied on mount. */
export function MixdownDialog({
  open,
  onOpenChange,
  recordings,
  channelId,
  initialSelection,
  onQueued,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  recordings: readonly T.Recording[];
  channelId: string;
  initialSelection: ReadonlySet<string>;
  onQueued: (r: T.Recording) => void;
}) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const mixdown = useMixdownMutation();
  const candidates = useMemo(() => mixdownCandidates(recordings, channelId), [recordings, channelId]);
  const [selected, setSelected] = useState<Set<string>>(() => new Set([...initialSelection].filter((id) => candidates.some((c) => c.id === id))));
  const [format, setFormat] = useState<T.RecordingFormat>("ogg_opus");
  const [stereo, setStereo] = useState(false);

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  const all = candidates.length > 0 && candidates.every((c) => selected.has(c.id));

  return (
    <FormDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t("recordings.mixdown")}
      description={t("recordings.mixdown.desc")}
      submitLabel={t("recordings.mixdown.run")}
      size="lg"
      disabled={candidates.length === 0 || mixdown.isPending}
      onSubmit={async () => {
        const r = await mixdown.mutateAsync({
          channel_id: channelId,
          sources: all ? undefined : [...selected],
          format,
          stereo,
        });
        toast.ok(t("recordings.mixdown.started"));
        onQueued(r);
      }}
    >
      <Field label={t("common.channel")}>
        <Mono>{channelId}</Mono>
      </Field>
      <Field
        label={t("recordings.sources")}
        hint={all ? t("recordings.mixdown.allTracks") : t("recordings.mixdown.selected", { n: selected.size })}
      >
        {candidates.length === 0 ? (
          <Callout tone="warn">{t("recordings.mixdown.noTracks")}</Callout>
        ) : (
          <div className="max-h-64 overflow-y-auto subtle-scroll rounded-xl border border-border divide-y divide-border">
            <label className="flex items-center gap-2 px-3 py-2 text-[12.5px] cursor-pointer">
              <input
                type="checkbox"
                className="size-3.5 accent-[var(--fg)] rounded"
                checked={all}
                onChange={(e) => setSelected(e.target.checked ? new Set(candidates.map((c) => c.id)) : new Set())}
              />
              <span className="font-medium">{t("common.all")}</span>
              <span className="ml-auto text-fg-faint">{candidates.length}</span>
            </label>
            {candidates.map((c) => (
              <label key={c.id} className="flex items-center gap-2 px-3 py-2 text-[12.5px] cursor-pointer">
                <input type="checkbox" className="size-3.5 accent-[var(--fg)] rounded" checked={selected.has(c.id)} onChange={() => toggle(c.id)} />
                <Mono title={c.user_id}>{shortId(c.user_id)}</Mono>
                <span className="text-fg-muted">{fmtDateTime(locale, c.started_at)}</span>
                <span className="ml-auto tabular text-fg-muted">{fmtDuration(locale, c.duration_secs)}</span>
              </label>
            ))}
          </div>
        )}
      </Field>
      <div className="grid grid-cols-2 gap-3">
        <Field label={t("recordings.mixdown.format")}>
          <NativeSelect value={format} onChange={(e) => setFormat(e.target.value === "wav" ? "wav" : "ogg_opus")}>
            <option value="ogg_opus">Ogg/Opus</option>
            <option value="wav">WAV (PCM)</option>
          </NativeSelect>
        </Field>
        <Field label={t("recordings.mixdown.stereo")} hint={t("recordings.mixdown.stereoHint")}>
          <Checkbox checked={stereo} onChange={(e) => setStereo(e.target.checked)} label={t("common.on")} />
        </Field>
      </div>
    </FormDialog>
  );
}
