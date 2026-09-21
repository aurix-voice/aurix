import { describe, expect, it } from "vitest";

import type { T } from "@/api/client";
import { filterRecordings, fmtOffset, isInFlight, mixdownCandidates, selectionChannel, speakerIndex, transcriptInFlight } from "@/pages/recordings/model";

const rec = (p: Partial<T.Recording> & Pick<T.Recording, "id">): T.Recording => ({
  app_id: "a",
  channel_id: "c1",
  session_id: "s",
  user_id: "u1",
  format: "ogg_opus",
  started_at: "2026-01-01T00:00:00Z",
  expires_at: "2026-02-01T00:00:00Z",
  kind: "recording",
  status: "ready",
  ...p,
});

const list = [
  rec({ id: "r1" }),
  rec({ id: "r2", user_id: "U2", status: "recording" }),
  rec({ id: "r3", channel_id: "c2", kind: "mixdown" }),
  rec({ id: "r4", kind: "evidence", status: "failed" }),
  rec({ id: "r5", status: "processing" }),
];

describe("recordings model", () => {
  it("in-flight detection drives polling", () => {
    expect(list.filter(isInFlight).map((r) => r.id)).toEqual(["r2", "r5"]);
    expect(transcriptInFlight(null)).toBe(false);
    expect(transcriptInFlight({ status: "queued" })).toBe(true);
    expect(transcriptInFlight({ status: "running" })).toBe(true);
    expect(transcriptInFlight({ status: "ready" })).toBe(false);
  });

  it("filters by kind, status and case-insensitive user prefix", () => {
    expect(filterRecordings(list, { kind: "", status: "", user: "" })).toHaveLength(5);
    expect(filterRecordings(list, { kind: "mixdown", status: "", user: "" }).map((r) => r.id)).toEqual(["r3"]);
    expect(filterRecordings(list, { kind: "", status: "failed", user: "" }).map((r) => r.id)).toEqual(["r4"]);
    expect(filterRecordings(list, { kind: "", status: "", user: " u2 " }).map((r) => r.id)).toEqual(["r2"]);
    expect(filterRecordings(list, { kind: "recording", status: "ready", user: "u" }).map((r) => r.id)).toEqual(["r1"]);
  });

  it("mixdown candidates are ready participant tracks of the channel only", () => {
    expect(mixdownCandidates(list, "c1").map((r) => r.id)).toEqual(["r1"]);
    expect(mixdownCandidates(list, "c2")).toEqual([]);
  });

  it("selection channel is the shared channel or null when mixed/empty", () => {
    expect(selectionChannel(list, new Set())).toBeNull();
    expect(selectionChannel(list, new Set(["r1", "r2"]))).toBe("c1");
    expect(selectionChannel(list, new Set(["r1", "r3"]))).toBeNull();
    expect(selectionChannel(list, new Set(["missing"]))).toBeNull();
  });

  it("offsets render as m:ss.mmm and roll into hours", () => {
    expect(fmtOffset(0)).toBe("0:00.000");
    expect(fmtOffset(61_250)).toBe("1:01.250");
    expect(fmtOffset(3_600_000 + 5_001)).toBe("1:00:05.001");
    expect(fmtOffset(-5)).toBe("0:00.000");
  });

  it("speaker index follows first appearance, unknown speaker is stable", () => {
    const seg = (speaker: string | null): T.TranscriptSegment => ({ speaker, start_ms: 0, end_ms: 1, text: "", language: "en", confidence: 1 });
    const idx = speakerIndex([seg("b"), seg(null), seg("a"), seg("b")]);
    expect([...idx.entries()]).toEqual([
      ["b", 0],
      ["?", 1],
      ["a", 2],
    ]);
  });
});
