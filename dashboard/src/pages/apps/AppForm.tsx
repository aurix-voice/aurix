import { useT } from "@/i18n";
import { Input, Textarea } from "@/ui/Input";
import { Field } from "@/ui/Primitives";

export interface AppFormValues {
  name: string;
  description: string;
  max_channels: string;
  max_participants_per_channel: string;
  max_concurrent_sessions: string;
  monthly_participant_minutes: string;
}

export const EMPTY_APP_FORM: AppFormValues = {
  name: "",
  description: "",
  max_channels: "",
  max_participants_per_channel: "",
  max_concurrent_sessions: "",
  monthly_participant_minutes: "",
};

/** Parses an optional non-negative integer field; empty → `undefined`, invalid → `null`. */
export function parseLimit(raw: string, min = 0): number | undefined | null {
  const s = raw.trim();
  if (!s) return undefined;
  if (!/^\d+$/.test(s)) return null;
  const v = Number(s);
  return Number.isSafeInteger(v) && v >= min ? v : null;
}

export function AppFormFields({
  value,
  onChange,
  autoFocus,
}: {
  value: AppFormValues;
  onChange: (v: AppFormValues) => void;
  autoFocus?: boolean;
}) {
  const t = useT();
  const set = (k: keyof AppFormValues) => (e: { currentTarget: { value: string } }) => onChange({ ...value, [k]: e.currentTarget.value });
  return (
    <div className="flex flex-col gap-3">
      <Field label={t("common.name")} required htmlFor="app-name">
        <Input id="app-name" value={value.name} onChange={set("name")} maxLength={128} autoFocus={autoFocus} required />
      </Field>
      <Field label={t("common.description")} hint={t("common.optional")} htmlFor="app-desc">
        <Textarea id="app-desc" value={value.description} onChange={set("description")} rows={2} />
      </Field>
      <div className="text-xs font-medium text-fg-muted pt-1">
        {t("apps.limits")} <span className="font-normal text-fg-faint">— {t("apps.limits.desc")}</span>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <Field label={t("apps.maxChannels")} htmlFor="app-mc">
          <Input id="app-mc" inputMode="numeric" value={value.max_channels} onChange={set("max_channels")} placeholder="—" />
        </Field>
        <Field label={t("apps.maxParticipantsPerChannel")} htmlFor="app-mp">
          <Input id="app-mp" inputMode="numeric" value={value.max_participants_per_channel} onChange={set("max_participants_per_channel")} placeholder="—" />
        </Field>
        <Field label={t("apps.maxConcurrentSessions")} hint={t("apps.maxConcurrentSessions.hint")} htmlFor="app-ccu">
          <Input id="app-ccu" inputMode="numeric" value={value.max_concurrent_sessions} onChange={set("max_concurrent_sessions")} placeholder="0" />
        </Field>
        <Field label={t("apps.monthlyParticipantMinutes")} hint={t("apps.monthlyParticipantMinutes.hint")} htmlFor="app-min">
          <Input id="app-min" inputMode="numeric" value={value.monthly_participant_minutes} onChange={set("monthly_participant_minutes")} placeholder="0" />
        </Field>
      </div>
    </div>
  );
}
