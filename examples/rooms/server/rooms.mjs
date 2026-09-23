/**
 * Room registry of the Rooms demo. A room is one Aurix channel named `rooms/<slug>`; the
 * registry only adds what Aurix does not store (title, profile, pin, last activity) and keeps
 * it in a small JSON file so restarts and the Aurix channel list agree.
 */

import { randomInt } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export const CHANNEL_PREFIX = "rooms/";
export const SLUG_RE = /^[a-z0-9]+(?:-[a-z0-9]+){0,3}$/;
export const MAX_TITLE = 48;

const ADJECTIVES = [
  "amber", "brisk", "calm", "clear", "cool", "crisp", "deep", "dusk", "early", "even", "fair", "fresh",
  "gentle", "glad", "green", "high", "keen", "kind", "late", "light", "lunar", "mild", "neat", "north",
  "open", "pale", "plain", "quiet", "rapid", "royal", "sharp", "silent", "soft", "solar", "still",
  "sunny", "swift", "tidy", "vivid", "warm", "wide", "wild", "young",
];
const NOUNS = [
  "arch", "bay", "beam", "bell", "birch", "bloom", "brook", "cedar", "cliff", "cloud", "coast", "cove",
  "creek", "crest", "dawn", "delta", "dune", "elm", "ember", "fern", "field", "fjord", "flare", "fox",
  "glen", "grove", "harbor", "hawk", "heath", "hill", "isle", "lake", "lark", "leaf", "marsh", "meadow",
  "moss", "oak", "orbit", "otter", "peak", "pine", "plume", "reef", "ridge", "river", "shore", "sky",
  "slope", "spruce", "stone", "storm", "summit", "tide", "trail", "vale", "wave", "willow", "wren",
];

/** `quiet-fox-417`: readable, typeable on a phone, ~5 M combinations. */
export function randomSlug() {
  const a = ADJECTIVES[randomInt(ADJECTIVES.length)];
  const n = NOUNS[randomInt(NOUNS.length)];
  return `${a}-${n}-${randomInt(100, 1000)}`;
}

/** Room profiles → Aurix `ChannelConfig`. `voice` is what a game lobby wants; `music` is the bot stage. */
export const PROFILES = Object.freeze({
  voice: Object.freeze({
    channel_type: "team",
    max_participants: 32,
    audio_profile: "voice",
    bitrate: 48000,
    min_bitrate: 16000,
    enable_dtx: true,
    enable_fec: true,
    max_bandwidth: "fullband",
  }),
  music: Object.freeze({
    channel_type: "team",
    max_participants: 64,
    audio_profile: "music",
    stereo: true,
    bitrate: 128000,
    min_bitrate: 48000,
    enable_dtx: false,
    enable_fec: true,
    max_bandwidth: "fullband",
  }),
});

export function normalizeTitle(raw) {
  if (typeof raw !== "string") return undefined;
  const title = raw.replace(/\s+/g, " ").trim();
  if (!title) return undefined;
  return title.length > MAX_TITLE ? title.slice(0, MAX_TITLE).trim() : title;
}

export function channelNameFor(slug) {
  return CHANNEL_PREFIX + slug;
}

export function slugFromChannelName(name) {
  if (typeof name !== "string" || !name.startsWith(CHANNEL_PREFIX)) return undefined;
  const slug = name.slice(CHANNEL_PREFIX.length);
  return SLUG_RE.test(slug) ? slug : undefined;
}

/**
 * In-memory map `slug → room` persisted to `file` (atomic rename). Rooms carry:
 * `{ slug, channelId, title, profile, pinned, createdAt, lastActiveAt }`.
 */
export class RoomStore {
  #file;
  #rooms = new Map();

  constructor(file) {
    this.#file = file;
    if (!file) return;
    let raw;
    try {
      raw = readFileSync(file, "utf8");
    } catch (err) {
      if (err.code === "ENOENT") return;
      throw err;
    }
    const parsed = JSON.parse(raw);
    for (const room of parsed.rooms ?? []) {
      if (SLUG_RE.test(room.slug) && typeof room.channelId === "string") this.#rooms.set(room.slug, room);
    }
  }

  get(slug) {
    return this.#rooms.get(slug);
  }

  bySlugOrChannel(id) {
    return this.#rooms.get(id) ?? [...this.#rooms.values()].find((r) => r.channelId === id);
  }

  list() {
    return [...this.#rooms.values()];
  }

  put(room) {
    this.#rooms.set(room.slug, room);
    this.#flush();
    return room;
  }

  delete(slug) {
    if (this.#rooms.delete(slug)) this.#flush();
  }

  touch(slug, at = Date.now()) {
    const room = this.#rooms.get(slug);
    if (!room) return;
    room.lastActiveAt = new Date(at).toISOString();
    this.#flush();
  }

  #flush() {
    if (!this.#file) return;
    mkdirSync(dirname(this.#file), { recursive: true });
    const tmp = `${this.#file}.tmp`;
    writeFileSync(tmp, JSON.stringify({ version: 1, rooms: this.list() }, null, 2));
    renameSync(tmp, this.#file);
  }
}
