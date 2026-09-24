/** Typed client for the rooms backend (`../server`). Everything voice-related goes to Aurix directly. */

export type TrackKind = "music" | "speech" | "ambience" | "signal";

export interface BotTrack {
  title: string;
  kind: TrackKind;
  artist?: string;
  license?: string;
  durationMs?: number;
  stereo?: boolean;
}

export interface BotStatus {
  room: string;
  track: BotTrack;
  /** Position inside the track when the status was sent, ms. */
  positionMs?: number;
  upNext?: BotTrack[];
  updatedAt: string;
}

export interface RoomCard {
  slug: string;
  title: string;
  profile: "voice" | "music";
  pinned: boolean;
  participants: number;
  maxParticipants: number;
  stereo: boolean;
  createdAt: string;
  bot: BotStatus | null;
}

export interface AppConfig {
  apiUrl: string;
  wsUrl: string;
  transport: "auto" | "webrtc" | "webtransport" | "websocket";
  stage: string;
  maxNameLength: number;
  maxTitleLength: number;
}

export interface JoinGrant {
  token: string;
  userId: string;
  expiresAt: string;
  channelId: string;
  room: RoomCard;
  apiUrl: string;
  wsUrl: string;
  transport: "auto" | "webrtc" | "webtransport" | "websocket";
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      accept: "application/json",
      ...(body !== undefined ? { "content-type": "application/json" } : {}),
      // ngrok's free plan shows an interstitial to requests that look like a browser page load;
      // this header opts API calls out of it and is ignored by every other proxy.
      "ngrok-skip-browser-warning": "1",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  let parsed: unknown = undefined;
  try {
    parsed = text ? JSON.parse(text) : undefined;
  } catch {
    parsed = undefined;
  }
  if (!res.ok) {
    const err = (parsed as { error?: { code?: string; message?: string } } | undefined)?.error;
    throw new ApiError(res.status, err?.code ?? "HTTP_ERROR", err?.message ?? `HTTP ${res.status}`);
  }
  return parsed as T;
}

/**
 * A TLS terminator that forwards plain HTTP without `X-Forwarded-Proto` (localhost.run, some
 * edges) leaves the backend advertising `http://` / `ws://` for the page's own host; a secure
 * page cannot open those, so the same-host URLs take the page's scheme.
 */
export function samePageScheme(url: string, page: Location = window.location): string {
  if (page.protocol !== "https:") return url;
  try {
    const u = new URL(url);
    if (u.host !== page.host) return url;
    if (u.protocol === "http:") u.protocol = "https:";
    else if (u.protocol === "ws:") u.protocol = "wss:";
    return u.toString().replace(/\/$/, url.endsWith("/") ? "/" : "");
  } catch {
    return url;
  }
}

function withPageScheme(grant: JoinGrant): JoinGrant {
  return { ...grant, apiUrl: samePageScheme(grant.apiUrl), wsUrl: samePageScheme(grant.wsUrl) };
}

export const api = {
  config: () => request<AppConfig>("GET", "/api/config"),
  room: (slug: string) => request<RoomCard>("GET", `/api/rooms/${encodeURIComponent(slug)}`),
  createRoom: (title: string | undefined) => request<RoomCard>("POST", "/api/rooms", { title, profile: "voice" }),
  join: (slug: string, name: string, deviceId: string) =>
    request<JoinGrant>("POST", `/api/rooms/${encodeURIComponent(slug)}/join`, { name, deviceId }).then(withPageScheme),
};

/** Live "now playing" of the bot for one room; `onStatus(null)` when it is silent or gone. */
export function subscribeBot(room: string, onStatus: (s: BotStatus | null) => void): () => void {
  const source = new EventSource(`/api/bot/events?room=${encodeURIComponent(room)}`);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const arm = () => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => onStatus(null), 35_000);
  };
  source.addEventListener("bot", (ev) => {
    const status = JSON.parse((ev as MessageEvent<string>).data) as BotStatus | null;
    onStatus(status && status.room === room ? status : null);
    arm();
  });
  arm();
  return () => {
    if (timer) clearTimeout(timer);
    source.close();
  };
}
