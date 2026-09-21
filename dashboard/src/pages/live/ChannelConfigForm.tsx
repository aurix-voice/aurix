import type { ReactNode } from "react";

import type { T } from "@/api/client";
import { useI18n, type MessageKey } from "@/i18n";
import { Checkbox, Input, NativeSelect, Textarea } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";
import { Callout, Field } from "@/ui/Primitives";

export const CHANNEL_TYPES: readonly T.ChannelType[] = ["team", "positional", "command", "whisper", "echo"];
export const CHANNEL_TYPE_KEY: Record<T.ChannelType, MessageKey> = {
  team: "live.type.team",
  positional: "live.type.positional",
  command: "live.type.command",
  whisper: "live.type.whisper",
  echo: "live.type.echo",
};
const BANDWIDTHS: readonly T.OpusBandwidth[] = ["narrowband", "mediumband", "wideband", "superwideband", "fullband"];
const PROFILES: readonly T.AudioProfile[] = ["voice", "music", "broadcast", "low_bandwidth"];
const SAMPLE_RATES = [8000, 12000, 16000, 24000, 48000] as const;
const ROLLOFFS: readonly T.RolloffCurve[] = ["linear", "logarithmic", "custom_spline"];

export type ConfigMode = "form" | "json";

/** Validated result of the JSON editor: parsed config or the parse error. */
export function parseConfigJson(text: string): { config: T.ChannelConfig } | { error: string } {
  try {
    const v: unknown = JSON.parse(text);
    if (typeof v !== "object" || v === null || Array.isArray(v)) return { error: "expected an object" };
    return { config: v };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
}

/** Client-side sanity checks mirroring the server's `ChannelConfig::validate`. */
export function configProblems(c: T.ChannelConfig): string[] {
  const out: string[] = [];
  if (c.bitrate !== undefined && (c.bitrate < 6000 || c.bitrate > 510_000)) out.push("bitrate");
  if (c.min_bitrate !== undefined && c.bitrate !== undefined && c.min_bitrate > c.bitrate) out.push("min_bitrate");
  if (c.max_participants !== undefined && c.max_participants < 1) out.push("max_participants");
  if (c.complexity !== undefined && c.complexity !== null && (c.complexity < 0 || c.complexity > 10)) out.push("complexity");
  if (c.e2ee && (c.recording_enabled || c.transcription || c.safety_voice || c.ambient || c.channel_type === "echo")) out.push("e2ee");
  if (c.audience?.max_speakers && c.max_participants !== undefined && c.audience.max_speakers > c.max_participants) out.push("audience.max_speakers");
  return out;
}

function num(v: string, fallback?: number): number | undefined {
  const s = v.trim();
  if (!s) return fallback;
  const n = Number(s);
  return Number.isFinite(n) ? n : fallback;
}

function Group({ title, children, hint }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <fieldset className="rounded-md border border-border p-3 flex flex-col gap-3">
      <legend className="px-1 text-xs font-medium text-fg-muted">{title}</legend>
      {hint ? <p className="text-xs text-fg-faint -mt-1">{hint}</p> : null}
      {children}
    </fieldset>
  );
}

/**
 * Channel configuration editor. `value` is the config object as the API represents it; the form
 * mode edits the known fields, JSON mode edits the whole object (unknown/future fields survive).
 */
export function ChannelConfigForm({
  value,
  onChange,
  mode,
  onModeChange,
  json,
  onJsonChange,
  jsonError,
  showType = true,
}: {
  value: T.ChannelConfig;
  onChange: (c: T.ChannelConfig) => void;
  mode: ConfigMode;
  onModeChange: (m: ConfigMode) => void;
  json: string;
  onJsonChange: (s: string) => void;
  jsonError: string | null;
  showType?: boolean;
}) {
  const { t } = useI18n();
  const set = <K extends keyof T.ChannelConfig>(k: K, v: T.ChannelConfig[K]) => onChange({ ...value, [k]: v });
  const problems = configProblems(value);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        <Segmented<ConfigMode>
          size="sm"
          value={mode}
          onChange={onModeChange}
          options={[
            { value: "form", label: t("live.config.form") },
            { value: "json", label: t("common.json") },
          ]}
        />
        {problems.length > 0 && mode === "form" ? (
          <span className="text-xs text-danger">{t("live.config.invalid", { fields: problems.join(", ") })}</span>
        ) : null}
      </div>

      {mode === "json" ? (
        <Field label={t("live.channelConfig")} error={jsonError} hint={t("live.config.json.hint")}>
          <Textarea value={json} onChange={(e) => onJsonChange(e.target.value)} className="font-mono text-xs min-h-72" spellCheck={false} />
        </Field>
      ) : (
        <>
          <Group title={t("live.config.general")}>
            <div className="grid grid-cols-2 gap-3">
              {showType ? (
                <Field label={t("live.channelType")}>
                  <NativeSelect value={value.channel_type ?? "team"} onChange={(e) => set("channel_type", e.target.value as T.ChannelType)}>
                    {CHANNEL_TYPES.map((ct) => (
                      <option key={ct} value={ct}>
                        {t(CHANNEL_TYPE_KEY[ct])}
                      </option>
                    ))}
                  </NativeSelect>
                </Field>
              ) : null}
              <Field label={t("live.maxParticipants")} htmlFor="channel-max-participants">
                <Input id="channel-max-participants" type="number" min={1} value={value.max_participants ?? ""} onChange={(e) => set("max_participants", num(e.target.value))} placeholder="256" />
              </Field>
              <Field label={t("live.audioProfile")}>
                <NativeSelect value={value.audio_profile ?? "voice"} onChange={(e) => set("audio_profile", e.target.value as T.AudioProfile)}>
                  {PROFILES.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </NativeSelect>
              </Field>
              <Field label={t("live.maxBandwidth")}>
                <NativeSelect value={value.max_bandwidth ?? "fullband"} onChange={(e) => set("max_bandwidth", e.target.value as T.OpusBandwidth)}>
                  {BANDWIDTHS.map((b) => (
                    <option key={b} value={b}>
                      {b}
                    </option>
                  ))}
                </NativeSelect>
              </Field>
              <Field label={t("live.bitrateBps")} hint="6000–510000">
                <Input type="number" min={6000} max={510000} step={1000} value={value.bitrate ?? ""} onChange={(e) => set("bitrate", num(e.target.value))} placeholder="48000" />
              </Field>
              <Field label={t("live.minBitrateBps")}>
                <Input type="number" min={6000} step={1000} value={value.min_bitrate ?? ""} onChange={(e) => set("min_bitrate", num(e.target.value))} placeholder="12000" />
              </Field>
              <Field label={t("live.sampleRate")}>
                <NativeSelect value={value.sample_rate ?? 48000} onChange={(e) => set("sample_rate", Number(e.target.value))}>
                  {SAMPLE_RATES.map((s) => (
                    <option key={s} value={s}>
                      {s} Hz
                    </option>
                  ))}
                </NativeSelect>
              </Field>
              <Field label={t("live.complexity")} hint={t("live.complexity.hint")}>
                <Input
                  type="number"
                  min={0}
                  max={10}
                  value={value.complexity ?? ""}
                  onChange={(e) => set("complexity", e.target.value.trim() === "" ? null : num(e.target.value, 0))}
                />
              </Field>
            </div>
            <div className="flex flex-wrap gap-x-5 gap-y-2">
              <Checkbox label={t("live.dtx")} checked={value.enable_dtx ?? true} onChange={(e) => set("enable_dtx", e.target.checked)} />
              <Checkbox label={t("live.fec")} checked={value.enable_fec ?? true} onChange={(e) => set("enable_fec", e.target.checked)} />
              <Checkbox label={t("live.stereo")} checked={value.stereo ?? false} onChange={(e) => set("stereo", e.target.checked)} />
              <Checkbox label={t("live.recording")} checked={value.recording_enabled ?? false} onChange={(e) => set("recording_enabled", e.target.checked)} />
              <Checkbox label={t("live.transcription")} checked={value.transcription ?? false} onChange={(e) => set("transcription", e.target.checked)} />
              <Checkbox label={t("live.safetyVoice")} checked={value.safety_voice ?? false} onChange={(e) => set("safety_voice", e.target.checked)} />
              <Checkbox label={t("live.e2ee")} checked={value.e2ee ?? false} onChange={(e) => set("e2ee", e.target.checked)} />
            </div>
            {value.e2ee ? <Callout tone="neutral">{t("live.e2ee.hint")}</Callout> : null}
          </Group>

          <Group title={t("live.positional")} hint={t("live.positional.hint")}>
            <Checkbox
              label={t("common.enabled")}
              checked={!!value.positional_config}
              onChange={(e) => set("positional_config", e.target.checked ? { ...(value.positional_config ?? {}) } : null)}
            />
            {value.positional_config ? (
              <PositionalFields value={value.positional_config} onChange={(p) => set("positional_config", p)} />
            ) : null}
          </Group>

          <Group title={t("live.ambient")} hint={t("live.ambient.hint")}>
            <Checkbox label={t("common.enabled")} checked={!!value.ambient} onChange={(e) => set("ambient", e.target.checked ? { ...(value.ambient ?? {}) } : null)} />
            {value.ambient ? (
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("live.ambient.maxVoices")}>
                  <Input type="number" min={0} value={value.ambient.max_voices ?? ""} onChange={(e) => set("ambient", { ...value.ambient, max_voices: num(e.target.value) })} placeholder="3" />
                </Field>
                <Field label={t("live.ambient.gain")}>
                  <Input type="number" min={0} max={1} step={0.05} value={value.ambient.ambient_gain ?? ""} onChange={(e) => set("ambient", { ...value.ambient, ambient_gain: num(e.target.value) })} placeholder="0.2" />
                </Field>
              </div>
            ) : null}
          </Group>

          <Group title={t("live.ducking")} hint={t("live.ducking.hint")}>
            <Checkbox label={t("common.enabled")} checked={!!value.ducking} onChange={(e) => set("ducking", e.target.checked ? { ...(value.ducking ?? {}) } : null)} />
            {value.ducking ? (
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("live.ducking.gain")}>
                  <Input type="number" min={0} max={1} step={0.05} value={value.ducking.gain ?? ""} onChange={(e) => set("ducking", { ...value.ducking, gain: num(e.target.value) })} placeholder="0.3" />
                </Field>
                <Field label={t("live.ducking.attack")}>
                  <Input type="number" min={0} value={value.ducking.attack_ms ?? ""} onChange={(e) => set("ducking", { ...value.ducking, attack_ms: num(e.target.value) })} placeholder="60" />
                </Field>
                <Field label={t("live.ducking.release")}>
                  <Input type="number" min={0} value={value.ducking.release_ms ?? ""} onChange={(e) => set("ducking", { ...value.ducking, release_ms: num(e.target.value) })} placeholder="400" />
                </Field>
                <Field label={t("live.ducking.hold")}>
                  <Input type="number" min={0} value={value.ducking.hold_ms ?? ""} onChange={(e) => set("ducking", { ...value.ducking, hold_ms: num(e.target.value) })} placeholder="300" />
                </Field>
                <Checkbox className="col-span-2" label={t("live.ducking.moderators")} checked={value.ducking.moderators ?? false} onChange={(e) => set("ducking", { ...value.ducking, moderators: e.target.checked })} />
              </div>
            ) : null}
          </Group>

          <Group title={t("live.audience")} hint={t("live.audience.hint")}>
            <Checkbox label={t("common.enabled")} checked={!!value.audience} onChange={(e) => set("audience", e.target.checked ? { ...(value.audience ?? {}) } : null)} />
            {value.audience ? (
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("live.audience.maxSpeakers")} hint={t("live.zeroUnlimited")}>
                  <Input type="number" min={0} value={value.audience.max_speakers ?? ""} onChange={(e) => set("audience", { ...value.audience, max_speakers: num(e.target.value) })} placeholder="0" />
                </Field>
                <Field label={t("live.audience.maxStreams")} hint={t("live.zeroUnlimited")}>
                  <Input type="number" min={0} value={value.audience.max_streams ?? ""} onChange={(e) => set("audience", { ...value.audience, max_streams: num(e.target.value) })} placeholder="0" />
                </Field>
                <Checkbox label={t("live.audience.hideListeners")} checked={value.audience.hide_listeners ?? false} onChange={(e) => set("audience", { ...value.audience, hide_listeners: e.target.checked })} />
                <Checkbox label={t("live.audience.mixForListeners")} checked={value.audience.mix_for_listeners ?? false} onChange={(e) => set("audience", { ...value.audience, mix_for_listeners: e.target.checked })} />
              </div>
            ) : null}
          </Group>

          {value.channel_type === "whisper" || value.channel_type === "command" ? (
            <Group title={t("live.config.roles")}>
              {value.channel_type === "whisper" ? (
                <Field label={t("live.whisperTarget")} hint={t("live.whisperTarget.hint")}>
                  <Input value={value.whisper_target ?? ""} onChange={(e) => set("whisper_target", e.target.value.trim() || null)} className="font-mono" />
                </Field>
              ) : (
                <Field label={t("live.commandSpeakers")} hint={t("live.commandSpeakers.hint")}>
                  <Textarea
                    value={(value.command_speakers ?? []).join("\n")}
                    onChange={(e) => {
                      const ids = e.target.value
                        .split(/[\s,]+/)
                        .map((s) => s.trim())
                        .filter(Boolean);
                      set("command_speakers", ids.length ? ids : null);
                    }}
                    className="font-mono text-xs"
                  />
                </Field>
              )}
            </Group>
          ) : null}
        </>
      )}
    </div>
  );
}

function PositionalFields({ value, onChange }: { value: T.PositionalConfig; onChange: (p: T.PositionalConfig) => void }) {
  const { t } = useI18n();
  const set = <K extends keyof T.PositionalConfig>(k: K, v: T.PositionalConfig[K]) => onChange({ ...value, [k]: v });
  return (
    <div className="grid grid-cols-2 gap-3">
      <Field label={t("live.positional.near")}>
        <Input type="number" min={0} step={0.5} value={value.near_distance ?? ""} onChange={(e) => set("near_distance", num(e.target.value))} placeholder="1" />
      </Field>
      <Field label={t("live.positional.far")}>
        <Input type="number" min={0} step={0.5} value={value.far_distance ?? ""} onChange={(e) => set("far_distance", num(e.target.value))} placeholder="50" />
      </Field>
      <Field label={t("live.positional.maxRadius")}>
        <Input type="number" min={0} step={0.5} value={value.max_radius ?? ""} onChange={(e) => set("max_radius", num(e.target.value))} />
      </Field>
      <Field label={t("live.positional.rolloff")}>
        <NativeSelect value={value.rolloff ?? "logarithmic"} onChange={(e) => set("rolloff", e.target.value as T.RolloffCurve)}>
          {ROLLOFFS.map((r) => (
            <option key={r} value={r}>
              {r}
            </option>
          ))}
        </NativeSelect>
      </Field>
      <Field label={t("live.positional.rosterRadius")} hint={t("live.positional.radius.hint")}>
        <Input
          type="number"
          min={0}
          step={0.5}
          value={value.roster_radius ?? ""}
          onChange={(e) => set("roster_radius", e.target.value.trim() === "" ? null : num(e.target.value))}
        />
      </Field>
      <Field label={t("live.positional.textRadius")} hint={t("live.positional.radius.hint")}>
        <Input
          type="number"
          min={0}
          step={0.5}
          value={value.text_radius ?? ""}
          onChange={(e) => set("text_radius", e.target.value.trim() === "" ? null : num(e.target.value))}
        />
      </Field>
      <Field label={t("live.positional.coords")}>
        <NativeSelect value={value.coordinate_system ?? "right_handed"} onChange={(e) => set("coordinate_system", e.target.value as T.CoordinateSystem)}>
          <option value="right_handed">right_handed</option>
          <option value="left_handed">left_handed</option>
        </NativeSelect>
      </Field>
      <Checkbox className="self-end pb-2" label={t("live.positional.directional")} checked={value.directional ?? false} onChange={(e) => set("directional", e.target.checked)} />
    </div>
  );
}
