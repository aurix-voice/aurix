import type { T } from "@/api/client";
import type { Point } from "@/components/charts";

export const ANALYTICS_TABS = ["usage", "quality", "channels"] as const;
export type AnalyticsTab = (typeof ANALYTICS_TABS)[number];

export function parseTab(v: string | undefined): AnalyticsTab {
  return ANALYTICS_TABS.find((t) => t === v) ?? "usage";
}

const at = (b: { bucket: string }) => Date.parse(b.bucket);

export function usagePoints(series: readonly T.UsageAppBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), sessions: b.peak_sessions, participants: b.peak_participants, minutes: b.participant_minutes, started: b.sessions_started }));
}

export function trafficPoints(series: readonly T.UsageAppBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), in: b.media_bytes_in, out: b.media_bytes_out, recording: b.recording_seconds }));
}

export function chatPoints(series: readonly T.UsageAppBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), chat: b.chat_messages, tts: b.tts_requests, stt: b.stt_audio_ms / 60_000 }));
}

export function qualityPoints(series: readonly T.UsageAppBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), mos: b.quality.mos_avg, loss: b.quality.loss_avg_percent, poor: b.quality.poor_percent }));
}

export function latencyPoints(series: readonly T.UsageAppBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), rtt: b.quality.rtt_avg_ms, jitter: b.quality.jitter_avg_ms, samples: b.quality_samples }));
}

export function channelPoints(series: readonly T.UsageChannelBucket[]): Point[] {
  return series.map((b) => ({ t: at(b), participants: b.peak_participants, minutes: b.participant_minutes, joins: b.joins, chat: b.chat_messages }));
}

/** Worst first: lowest mean MOS, ties broken by more poor time, then more alerts. */
export function rankSessions(sessions: readonly T.SessionQuality[]): T.SessionQuality[] {
  return sessions.slice().sort((a, b) => a.quality.mos_avg - b.quality.mos_avg || b.quality.poor_seconds - a.quality.poor_seconds || b.quality.mos_alerts - a.quality.mos_alerts);
}

export type ChannelSortKey = "participant_minutes" | "peak_participants" | "joins" | "unique_users" | "chat_messages";

export function rankChannels(channels: readonly T.ChannelUsageTotals[], key: ChannelSortKey): T.ChannelUsageTotals[] {
  return channels.slice().sort((a, b) => b[key] - a[key] || a.channel_id.localeCompare(b.channel_id));
}

/** Session bar histogram (index 0 = 0 bars … 4 = 4 bars) as percentages of rated time. */
export function barsPercent(bars: readonly number[]): number[] {
  const total = bars.reduce((s, n) => s + n, 0);
  return total === 0 ? bars.map(() => 0) : bars.map((n) => (n / total) * 100);
}

export function exportFilename(scope: "app" | "channels", format: "json" | "csv", range: { from: string; to: string }): string {
  const day = (iso: string) => iso.slice(0, 10);
  return `aurix-usage-${scope}-${day(range.from)}_${day(range.to)}.${format}`;
}
