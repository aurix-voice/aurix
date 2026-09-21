import type { T } from "@/api/client";
import { useI18n } from "@/i18n";
import { Badge, Tip } from "@/ui/Primitives";

import { CHANNEL_TYPE_KEY } from "./ChannelConfigForm";

export function ChannelTypeBadge({ type }: { type: string }) {
  const { t } = useI18n();
  const key = (Object.keys(CHANNEL_TYPE_KEY) as T.ChannelType[]).find((k) => k === type);
  return <Badge tone={type === "positional" ? "accent" : "neutral"}>{key ? t(CHANNEL_TYPE_KEY[key]) : type}</Badge>;
}

/** Feature flags of a channel configuration as compact badges. */
export function ChannelFlags({ config, adHoc, persistent }: { config: T.ChannelConfig; adHoc?: boolean; persistent?: boolean }) {
  const { t } = useI18n();
  const flags: Array<{ k: string; label: string; tip: string; tone?: "neutral" | "accent" | "warn" }> = [];
  if (config.e2ee) flags.push({ k: "e2ee", label: "E2EE", tip: t("live.e2ee"), tone: "accent" });
  if (config.recording_enabled) flags.push({ k: "rec", label: "REC", tip: t("live.recording"), tone: "warn" });
  if (config.transcription) flags.push({ k: "stt", label: "STT", tip: t("live.transcription") });
  if (config.safety_voice) flags.push({ k: "safety", label: "SAFE", tip: t("live.safetyVoice") });
  if (config.stereo) flags.push({ k: "stereo", label: "2ch", tip: t("live.stereo") });
  if (config.positional_config) flags.push({ k: "pos", label: "3D", tip: t("live.positional") });
  if (config.ambient) flags.push({ k: "amb", label: "AMB", tip: t("live.ambient") });
  if (config.audience) flags.push({ k: "aud", label: "AUD", tip: t("live.audience") });
  if (config.ducking) flags.push({ k: "duck", label: "DUCK", tip: t("live.ducking") });
  if (adHoc) flags.push({ k: "adhoc", label: t("live.adHoc"), tip: t("live.adHoc.hint") });
  else if (persistent) flags.push({ k: "persist", label: t("live.persistent"), tip: t("live.persistent.hint") });
  if (flags.length === 0) return <span className="text-fg-faint">—</span>;
  return (
    <span className="inline-flex flex-wrap items-center gap-1">
      {flags.map((f) => (
        <Tip key={f.k} content={f.tip}>
          <span className="inline-flex">
            <Badge tone={f.tone ?? "neutral"}>{f.label}</Badge>
          </span>
        </Tip>
      ))}
    </span>
  );
}
