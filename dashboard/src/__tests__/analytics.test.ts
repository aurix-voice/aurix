import { describe, expect, it } from "vitest";

import type { T } from "@/api/client";
import { barsPercent, chatPoints, exportFilename, parseTab, qualityPoints, rankChannels, rankSessions, usagePoints } from "@/pages/analytics/model";

const bucket = (p: Partial<T.UsageAppBucket> & { bucket: string }): T.UsageAppBucket => ({
  app_id: "a",
  peak_sessions: 0,
  session_minutes: 0,
  sessions_started: 0,
  unique_users: 0,
  peak_participants: 0,
  participant_minutes: 0,
  active_channels: 0,
  recording_seconds: 0,
  media_bytes_in: 0,
  media_bytes_out: 0,
  chat_messages: 0,
  tts_requests: 0,
  tts_characters: 0,
  stt_audio_ms: 0,
  quality_samples: 0,
  mos_sum_milli: 0,
  rtt_sum_ms: 0,
  jitter_sum_ms: 0,
  loss_sum_permille: 0,
  poor_quality_samples: 0,
  updated_at: p.bucket,
  quality: { samples: 0, mos_avg: null, rtt_avg_ms: null, jitter_avg_ms: null, loss_avg_percent: null, poor_samples: 0, poor_percent: null },
  ...p,
});

const session = (id: string, q: Partial<T.QualitySummary>): T.SessionQuality => ({
  session_id: id,
  user_id: "u",
  media_node_id: null,
  connected_at: "2026-01-01T00:00:00Z",
  disconnected_at: null,
  disconnect_reason: null,
  quality: {
    samples: 10,
    seconds: 100,
    mos_avg: 4,
    mos_min: 3.5,
    mos_last: 4,
    r_factor_avg: 80,
    rtt_avg_ms: 40,
    rtt_max_ms: 60,
    jitter_avg_ms: 3,
    loss_avg_percent: 0.5,
    loss_max_percent: 2,
    bars: [0, 0, 0, 20, 80],
    poor_seconds: 0,
    mos_alerts: 0,
    ...q,
  },
});

describe("analytics model", () => {
  it("parseTab falls back to usage", () => {
    expect(parseTab(undefined)).toBe("usage");
    expect(parseTab("quality")).toBe("quality");
    expect(parseTab("fleet")).toBe("usage");
  });

  it("series map to epoch points and keep server-derived nulls", () => {
    const s = [
      bucket({ bucket: "2026-01-01T00:00:00Z", peak_sessions: 3, peak_participants: 5, participant_minutes: 12, sessions_started: 7, chat_messages: 4, tts_requests: 1, stt_audio_ms: 90_000 }),
      bucket({ bucket: "2026-01-01T01:00:00Z", quality_samples: 2, quality: { samples: 2, mos_avg: 4.1, rtt_avg_ms: 30, jitter_avg_ms: 2, loss_avg_percent: 1.5, poor_samples: 0, poor_percent: 0 } }),
    ];
    expect(usagePoints(s)[0]).toEqual({ t: Date.parse("2026-01-01T00:00:00Z"), sessions: 3, participants: 5, minutes: 12, started: 7 });
    expect(chatPoints(s)[0]).toEqual({ t: Date.parse("2026-01-01T00:00:00Z"), chat: 4, tts: 1, stt: 1.5 });
    const q = qualityPoints(s);
    expect(q[0]).toEqual({ t: Date.parse("2026-01-01T00:00:00Z"), mos: null, loss: null, poor: null });
    expect(q[1]).toEqual({ t: Date.parse("2026-01-01T01:00:00Z"), mos: 4.1, loss: 1.5, poor: 0 });
  });

  it("rankSessions: lowest MOS first, then more poor time, then more alerts", () => {
    const ranked = rankSessions([
      session("good", { mos_avg: 4.2 }),
      session("bad-a", { mos_avg: 2.5, poor_seconds: 10, mos_alerts: 0 }),
      session("bad-b", { mos_avg: 2.5, poor_seconds: 30, mos_alerts: 1 }),
      session("bad-c", { mos_avg: 2.5, poor_seconds: 30, mos_alerts: 3 }),
    ]);
    expect(ranked.map((s) => s.session_id)).toEqual(["bad-c", "bad-b", "bad-a", "good"]);
  });

  it("rankChannels sorts by the chosen key descending with a stable id tiebreak", () => {
    const ch = (channel_id: string, participant_minutes: number, joins: number): T.ChannelUsageTotals => ({
      channel_id,
      peak_participants: 0,
      participant_minutes,
      joins,
      unique_users: 0,
      chat_messages: 0,
      tts_requests: 0,
      tts_characters: 0,
      stt_audio_ms: 0,
    });
    const list = [ch("b", 10, 1), ch("a", 10, 5), ch("c", 20, 2)];
    expect(rankChannels(list, "participant_minutes").map((c) => c.channel_id)).toEqual(["c", "a", "b"]);
    expect(rankChannels(list, "joins").map((c) => c.channel_id)).toEqual(["a", "c", "b"]);
    expect(list.map((c) => c.channel_id)).toEqual(["b", "a", "c"]);
  });

  it("barsPercent normalises to 100 and survives an empty histogram", () => {
    expect(barsPercent([0, 0, 0, 25, 75])).toEqual([0, 0, 0, 25, 75]);
    expect(barsPercent([1, 1, 2, 0, 0])).toEqual([25, 25, 50, 0, 0]);
    expect(barsPercent([0, 0, 0, 0, 0])).toEqual([0, 0, 0, 0, 0]);
  });

  it("export filenames carry scope, format and the day range", () => {
    expect(exportFilename("channels", "csv", { from: "2026-01-01T10:00:00Z", to: "2026-01-08T10:00:00Z" })).toBe("aurix-usage-channels-2026-01-01_2026-01-08.csv");
  });
});
