import { type QueryKey, useQueryClient } from "@tanstack/react-query";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import { useAuth } from "@/auth/AuthProvider";

import { AurixError, makeClient, type T } from "./client";
import { qk } from "./hooks";
import { useAppScope } from "./scope";

export type StreamState = "idle" | "connecting" | "live" | "reconnecting" | "unauthorized";

export interface LiveEvent {
  id: string | undefined;
  type: string;
  envelope: T.EventEnvelope | null;
  receivedAt: number;
}

type Listener = (ev: LiveEvent) => void;

interface EventsCtx {
  state: StreamState;
  /** Increments on every `lagged` marker so panels can re-snapshot. */
  lagged: number;
  lastEventAt: number | null;
  subscribe: (fn: Listener) => () => void;
}

const Ctx = createContext<EventsCtx | null>(null);

/** Everything but the high-frequency noise (typing/speaking/energy). */
export const DEFAULT_TYPES = [
  "channel.created",
  "channel.destroyed",
  "channel.config_updated",
  "channel.activated",
  "channel.deactivated",
  "participant.joined",
  "participant.left",
  "participant.muted",
  "participant.unmuted",
  "participant.priority_changed",
  "participant.kicked",
  "user.banned",
  "user.deleted",
  "user.block_changed",
  "moderation.event",
  "recording.started",
  "recording.stopped",
  "recording.consent_required",
  "recording.processed",
  "audio_stream.started",
  "audio_stream.stopped",
  "quality.alert",
  "quality.recovered",
  "chat.message",
  "chat.message_updated",
  "chat.reaction",
  "channel.transcript",
  "tts.status",
  "safety.incident",
  "safety.risk_changed",
  "participant.speaking",
];

/** Which cached queries an event type makes stale. */
function invalidationsFor(app: string, type: string, data: Record<string, unknown>): QueryKey[] {
  const channelId = typeof data.channel_id === "string" ? data.channel_id : null;
  const userId = typeof data.user_id === "string" ? data.user_id : null;
  const head = type.split(".")[0];
  switch (head) {
    case "channel":
      if (type === "channel.destroyed") return [qk.channelLists(app)];
      return channelId ? [qk.channels(app), qk.channel(app, channelId)] : [qk.channels(app)];
    case "participant":
      if (type === "participant.speaking") return [];
      return channelId
        ? [qk.channels(app), qk.channelParticipants(app, channelId), qk.sessions(app)]
        : [qk.channels(app), qk.sessions(app)];
    case "user":
      return userId ? [qk.user(app, userId), qk.userBlocks(app, userId), ["t", app, "users"], ["t", app, "bans"]] : [["t", app, "users"]];
    case "moderation":
      return [["t", app, "moderation"], ["t", app, "bans"], ["admin", "moderation"]];
    case "safety":
      return [["t", app, "incidents"], ["t", app, "moderation"], userId ? qk.userRisk(app, userId) : ["t", app, "users"]];
    case "recording":
      return [["t", app, "recordings"], channelId ? qk.channelRecordings(app, channelId) : ["t", app, "recordings"]];
    case "quality":
      return [qk.sessions(app)];
    case "chat":
      return [["t", app, "chat"]];
    default:
      return [];
  }
}

export function EventsProvider({ children }: { children: ReactNode }) {
  const { token, status } = useAuth();
  const { appId } = useAppScope();
  const qc = useQueryClient();
  const [conn, setConn] = useState<StreamState>("connecting");
  const enabled = status === "authenticated" && !!token && !!appId;
  const state: StreamState = enabled ? conn : "idle";
  const [lagged, setLagged] = useState(0);
  const [lastEventAt, setLastEventAt] = useState<number | null>(null);
  const listeners = useRef(new Set<Listener>());

  const subscribe = useCallback((fn: Listener) => {
    listeners.current.add(fn);
    return () => {
      listeners.current.delete(fn);
    };
  }, []);

  useEffect(() => {
    if (!enabled || !token || !appId) return;
    const app = appId;
    const ctrl = new AbortController();
    const client = makeClient(token, app);
    // Debounced invalidation: bursts of joins/leaves collapse into one refetch per key.
    const pending = new Map<string, readonly unknown[]>();
    let flushTimer: ReturnType<typeof setTimeout> | null = null;
    const flush = () => {
      flushTimer = null;
      for (const key of pending.values()) void qc.invalidateQueries({ queryKey: key });
      pending.clear();
    };
    const schedule = (keys: readonly QueryKey[]) => {
      for (const k of keys) pending.set(JSON.stringify(k), k);
      if (!flushTimer) flushTimer = setTimeout(flush, 250);
    };

    void (async () => {
      setConn("connecting");
      try {
        for await (const ev of client.events({
          signal: ctrl.signal,
          types: DEFAULT_TYPES,
          reconnectDelayMs: 1000,
          maxReconnectDelayMs: 15_000,
        })) {
          if (ctrl.signal.aborted) break;
          if (ev.type === "stream.open") {
            setConn("live");
            continue;
          }
          if (ev.type === "lagged") {
            setLagged((n) => n + 1);
            void qc.invalidateQueries({ queryKey: qk.tenant(app) });
            continue;
          }
          setConn("live");
          const now = Date.now();
          setLastEventAt(now);
          const envelope = isEnvelope(ev.data) ? ev.data : null;
          const live: LiveEvent = { id: ev.id, type: ev.type, envelope, receivedAt: now };
          schedule(invalidationsFor(app, ev.type, envelope?.data ?? {}));
          for (const l of listeners.current) l(live);
        }
      } catch (err) {
        if (ctrl.signal.aborted) return;
        if (err instanceof AurixError && (err.status === 401 || err.status === 403)) {
          setConn("unauthorized");
        } else {
          setConn("reconnecting");
        }
      }
    })();

    // The SDK flips to reconnecting silently; surface it by watching for stream.open gaps.
    return () => {
      ctrl.abort();
      if (flushTimer) clearTimeout(flushTimer);
    };
  }, [enabled, token, appId, qc]);

  const value = useMemo(() => ({ state, lagged, lastEventAt, subscribe }), [state, lagged, lastEventAt, subscribe]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

function isEnvelope(v: unknown): v is T.EventEnvelope {
  return typeof v === "object" && v !== null && "id" in v && "type" in v && "data" in v;
}

export function useEvents(): EventsCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useEvents outside EventsProvider");
  return ctx;
}

/** Subscribes to live events for the lifetime of the component. */
export function useServerEvents(handler: Listener, deps: readonly unknown[] = []): void {
  const { subscribe } = useEvents();
  const ref = useRef(handler);
  useEffect(() => {
    ref.current = handler;
  }, [handler]);
  useEffect(() => subscribe((ev) => ref.current(ev)), [subscribe, ...deps]); // eslint-disable-line react-hooks/exhaustive-deps
}
