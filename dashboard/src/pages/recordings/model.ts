import type { T } from "@/api/client";
import type { Tone } from "@/ui/Primitives";

export const RECORDING_KINDS: readonly T.RecordingKind[] = ["recording", "evidence", "mixdown"];
export const RECORDING_STATUSES: readonly T.RecordingStatus[] = ["recording", "processing", "ready", "failed"];
export const TRANSCRIPT_FORMATS: readonly T.GetRecordingTranscriptFormat[] = ["json", "srt", "vtt"];

export function statusTone(status: T.RecordingStatus): Tone {
  switch (status) {
    case "ready":
      return "ok";
    case "failed":
      return "danger";
    case "recording":
      return "accent";
    case "processing":
      return "warn";
  }
}

export function transcriptTone(status: T.RecordingTranscriptStatus): Tone {
  switch (status) {
    case "ready":
      return "ok";
    case "failed":
      return "danger";
    case "queued":
      return "neutral";
    case "running":
      return "warn";
  }
}

/** File extension of a stored recording. */
export function fileExt(format: T.RecordingFormat): "wav" | "ogg" {
  return format === "wav" ? "wav" : "ogg";
}

export function isInFlight(r: Pick<T.Recording, "status">): boolean {
  return r.status === "recording" || r.status === "processing";
}

export function transcriptInFlight(tr: Pick<T.RecordingTranscript, "status"> | null | undefined): boolean {
  return !!tr && (tr.status === "queued" || tr.status === "running");
}

export interface RecordingFilter {
  kind: T.RecordingKind | "";
  status: T.RecordingStatus | "";
  user: string;
}

export function filterRecordings(list: readonly T.Recording[], f: RecordingFilter): T.Recording[] {
  const user = f.user.trim().toLowerCase();
  return list.filter(
    (r) => (!f.kind || r.kind === f.kind) && (!f.status || r.status === f.status) && (!user || r.user_id.toLowerCase().startsWith(user)),
  );
}

/** Participant tracks that can feed a mixdown of `channelId` (ready, same channel). */
export function mixdownCandidates(list: readonly T.Recording[], channelId: string): T.Recording[] {
  return list.filter((r) => r.channel_id === channelId && r.kind === "recording" && r.status === "ready");
}

/** Channel of the current selection when all selected tracks share one; `null` otherwise. */
export function selectionChannel(list: readonly T.Recording[], selected: ReadonlySet<string>): string | null {
  let channel: string | null = null;
  for (const r of list) {
    if (!selected.has(r.id)) continue;
    if (channel === null) channel = r.channel_id;
    else if (channel !== r.channel_id) return null;
  }
  return channel;
}

/** `mm:ss.mmm` offset on the recording timeline. */
export function fmtOffset(ms: number): string {
  const total = Math.max(0, Math.round(ms));
  const h = Math.floor(total / 3_600_000);
  const m = Math.floor((total % 3_600_000) / 60_000);
  const s = Math.floor((total % 60_000) / 1000);
  const frac = total % 1000;
  const mm = h > 0 ? `${h}:${String(m).padStart(2, "0")}` : String(m);
  return `${mm}:${String(s).padStart(2, "0")}.${String(frac).padStart(3, "0")}`;
}

/** Speakers in first-appearance order; `null` speaker becomes `"?"`. */
export function speakerIndex(segments: readonly T.TranscriptSegment[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const s of segments) {
    const key = s.speaker ?? "?";
    if (!m.has(key)) m.set(key, m.size);
  }
  return m;
}
