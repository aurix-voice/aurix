import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type FormEvent } from "react";

import { ApiError, api, subscribeBot, type BotStatus, type RoomCard } from "../api";
import { Avatar, Bars, Center, Equalizer, Icon, IconButton } from "../components";
import { deviceId, saveName, savedName } from "../identity";
import { useT } from "../i18n";
import { navigate } from "../router";
import { RoomController, type Person, type Snapshot } from "../room-controller";

const IDLE: Snapshot = {
  phase: "connecting",
  people: [],
  muted: false,
  outputMuted: false,
  micAvailable: true,
  messages: [],
  typing: [],
  needsAudioTap: false,
};

export function Room({ slug, autoJoin }: { slug: string; autoJoin: boolean }) {
  const { t } = useT();
  const [card, setCard] = useState<RoomCard>();
  const [missing, setMissing] = useState(false);
  const [name, setName] = useState(savedName);
  const [controller, setController] = useState<RoomController>();
  const [joinError, setJoinError] = useState<string>();
  const [joining, setJoining] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .room(slug)
      .then((c) => alive && setCard(c))
      .catch((e: unknown) => alive && e instanceof ApiError && e.status === 404 && setMissing(true));
    return () => {
      alive = false;
    };
  }, [slug]);

  useEffect(() => {
    document.title = card ? `${card.title} — Aurix Rooms` : "Aurix Rooms";
    return () => {
      document.title = "Aurix Rooms";
    };
  }, [card]);

  const join = async () => {
    const trimmed = name.trim();
    if (!trimmed || joining) return;
    setJoining(true);
    setJoinError(undefined);
    try {
      saveName(trimmed);
      const grant = await api.join(slug, trimmed, deviceId());
      const c = new RoomController(slug, trimmed, deviceId(), grant);
      setController(c);
      void c.start();
    } catch (e) {
      setJoinError(e instanceof ApiError && e.status === 429 ? t.rateLimited : e instanceof ApiError && e.status === 404 ? t.roomNotFound : t.errorGeneric);
      setJoining(false);
    }
  };

  const autoJoined = useRef(false);
  useEffect(() => {
    if (autoJoin && !autoJoined.current && name.trim()) {
      autoJoined.current = true;
      void join();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoJoin]);

  useEffect(() => () => controller?.leave(), [controller]);
  useEffect(() => {
    const bye = () => controller?.leave();
    addEventListener("pagehide", bye);
    return () => removeEventListener("pagehide", bye);
  }, [controller]);

  if (missing) {
    return (
      <Center title={t.roomNotFound}>
        <button type="button" className="btn ghost" onClick={() => navigate("/")}>
          {t.back}
        </button>
      </Center>
    );
  }

  if (!controller) {
    return (
      <main className="page lobby">
        <header className="brand">
          <div>
            <h1>{card?.title ?? "…"}</h1>
            <p className="tagline">{card ? t.inRoom(card.participants) : ""}</p>
          </div>
          <IconButton icon="back" label={t.back} onClick={() => navigate("/")} />
        </header>
        <form
          className="section"
          onSubmit={(e: FormEvent) => {
            e.preventDefault();
            void join();
          }}
        >
          <label className="label" htmlFor="gate-name">
            {t.yourName}
          </label>
          <input
            id="gate-name"
            className="field"
            value={name}
            maxLength={32}
            autoComplete="nickname"
            placeholder={t.namePlaceholder}
            onChange={(e) => setName(e.target.value)}
          />
          <button type="submit" className="btn block" disabled={!name.trim() || joining || !card}>
            {joining ? t.joining : t.join}
          </button>
          {joinError && (
            <p className="hint bad" role="alert">
              {joinError}
            </p>
          )}
        </form>
      </main>
    );
  }

  return <Live slug={slug} card={card} controller={controller} onLeft={() => navigate("/")} />;
}

function Live({ slug, card, controller, onLeft }: { slug: string; card: RoomCard | undefined; controller: RoomController; onLeft: () => void }) {
  const { t } = useT();
  const snap = useSyncExternalStore(controller.subscribe, controller.getSnapshot, () => IDLE);
  const [chatOpen, setChatOpen] = useState(false);
  const [unread, setUnread] = useState(0);
  const [copied, setCopied] = useState(false);
  const [bot, setBot] = useState<BotStatus | null>(null);
  const seen = useRef(0);

  useEffect(() => subscribeBot(slug, setBot), [slug]);

  useEffect(() => {
    if (chatOpen) {
      seen.current = snap.messages.length;
      setUnread(0);
    } else {
      setUnread(snap.messages.filter((m, i) => i >= seen.current && !m.own).length);
    }
  }, [snap.messages, chatOpen]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(location.href);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: the URL bar still has it */
    }
  };

  if (snap.phase === "left" || snap.phase === "kicked" || snap.phase === "failed") {
    const title = snap.phase === "left" ? t.leftRoom : snap.phase === "kicked" ? t.kicked : t.failed;
    return (
      <Center title={title}>
        {snap.phase === "failed" && snap.error && <p className="hint">{snap.error}</p>}
        {snap.phase === "failed" && (
          <button type="button" className="btn" onClick={() => location.reload()}>
            {t.retry}
          </button>
        )}
        <button type="button" className="btn ghost" onClick={onLeft}>
          {t.back}
        </button>
      </Center>
    );
  }

  const q = snap.quality;
  const qualityTitle = q
    ? `${t.quality}: ${t.qualityLabels[Math.max(1, Math.min(5, Math.round(q.bars)))]} · ${t.rtt} ${Math.round(q.rttMs)} ms · ${t.loss} ${q.downlinkLossPercent.toFixed(1)}% · ${t.jitter} ${Math.round(q.downlinkJitterMs)} ms`
    : t.quality;
  const status = snap.phase === "connecting" ? t.connecting : snap.phase === "reconnecting" ? t.reconnecting : undefined;
  const botPerson = snap.people.find((p) => p.bot);
  const others = snap.people.filter((p) => !p.bot);

  return (
    <div className={`room ${chatOpen ? "with-chat" : ""}`}>
      <header className="topbar">
        <div className="name">
          <b>{card?.title ?? slug}</b>
          <span>
            <span>{slug}</span>
            {snap.transport && <span>{t.transport[snap.transport]}</span>}
            {status && <span>{status}</span>}
          </span>
        </div>
        <Bars bars={q?.bars} title={qualityTitle} />
        <IconButton icon={copied ? "check" : "link"} label={copied ? t.copied : t.copyLink} onClick={() => void copy()} />
        <IconButton icon="chat" label={t.chat} onClick={() => setChatOpen((v) => !v)} on={chatOpen} dot={unread > 0} className="desktop-only" />
      </header>

      <main className="grid" aria-live="polite">
        {botPerson && <BotTile person={botPerson} status={bot} />}
        {others.map((p) => (
          <Tile key={p.userId} person={p} />
        ))}
        {others.length <= 1 && !botPerson && snap.phase === "connected" && <p className="empty">{t.empty}</p>}
      </main>

      <footer className="bottombar">
        <IconButton
          icon={snap.muted || !snap.micAvailable ? "micOff" : "mic"}
          label={snap.muted ? t.unmute : t.mute}
          on={snap.muted || !snap.micAvailable}
          disabled={!snap.micAvailable}
          onClick={() => controller.setMuted(!snap.muted)}
        />
        <IconButton
          icon={snap.outputMuted ? "speakerOff" : "speaker"}
          label={snap.outputMuted ? t.speakerOff : t.speakerOn}
          on={snap.outputMuted}
          onClick={() => controller.setOutputMuted(!snap.outputMuted)}
        />
        <IconButton icon="chat" label={t.chat} on={chatOpen} dot={unread > 0} onClick={() => setChatOpen((v) => !v)} className="mobile-only" />
        <IconButton icon="leave" label={t.leave} danger onClick={() => controller.leave()} />
      </footer>

      {chatOpen && <Chat snap={snap} controller={controller} onClose={() => setChatOpen(false)} />}

      {snap.needsAudioTap && (
        <div className="banner" role="status">
          <span>{t.tapToHear}</span>
          <button type="button" onClick={() => void controller.tapAudio()}>
            OK
          </button>
        </div>
      )}
      {!snap.micAvailable && snap.phase === "connected" && !snap.needsAudioTap && (
        <div className="banner" role="status">
          {t.noMicWarn}
        </div>
      )}
    </div>
  );
}

function Tile({ person }: { person: Person }) {
  const { t } = useT();
  return (
    <div className={`tile ${person.speaking ? "speaking" : ""}`}>
      <Avatar id={person.userId} name={person.name} />
      <span className="who">
        {person.name}
        {person.self && <small>({t.you})</small>}
      </span>
      {person.muted && (
        <span className="mutedmark" title={t.muted}>
          <Icon name="micOff" />
        </span>
      )}
    </div>
  );
}

function BotTile({ person, status }: { person: Person; status: BotStatus | null }) {
  const { t } = useT();
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const progress = useMemo(() => {
    if (!status?.track.durationMs) return undefined;
    const elapsed = (status.positionMs ?? 0) + (now - Date.parse(status.updatedAt));
    return Math.max(0, Math.min(1, elapsed / status.track.durationMs));
  }, [status, now]);
  const sub = status
    ? [t.kinds[status.track.kind], status.track.artist, status.track.stereo ? t.stereo : t.mono].filter(Boolean).join(" · ")
    : t.botOffline;
  return (
    <div className={`tile bot ${person.speaking ? "speaking" : ""}`}>
      <Avatar id={person.userId} name={person.name} bot />
      <div className="info">
        <span className="t">{status ? status.track.title : person.name}</span>
        <span className="s" title={sub}>
          {person.speaking && status && (
            <>
              <Equalizer />{" "}
            </>
          )}
          {sub}
        </span>
        {progress !== undefined && (
          <span className="progress" aria-hidden="true">
            <i style={{ width: `${progress * 100}%` }} />
          </span>
        )}
        {status?.upNext?.[0] && (
          <span className="s">
            {t.upNext}: {status.upNext[0].title}
          </span>
        )}
      </div>
    </div>
  );
}

function Chat({ snap, controller, onClose }: { snap: Snapshot; controller: RoomController; onClose: () => void }) {
  const { t, lang } = useT();
  const [text, setText] = useState("");
  const log = useRef<HTMLDivElement>(null);
  const lastTyping = useRef(0);

  useEffect(() => {
    log.current?.scrollTo({ top: log.current.scrollHeight });
  }, [snap.messages.length]);

  const send = async (ev: FormEvent) => {
    ev.preventDefault();
    const value = text;
    if (!value.trim()) return;
    setText("");
    try {
      await controller.send(value);
    } catch {
      setText(value);
    }
  };

  const typingNames = snap.typing
    .map((id) => snap.people.find((p) => p.userId === id)?.name)
    .filter((n): n is string => Boolean(n));
  const time = new Intl.DateTimeFormat(lang, { hour: "2-digit", minute: "2-digit" });

  return (
    <aside className="chat" aria-label={t.chat}>
      <div className="chead">
        <span>{t.chat}</span>
        <IconButton icon="close" label={t.back} onClick={onClose} />
      </div>
      <div className="log" ref={log}>
        {snap.messages.map((m) => (
          <div key={m.id} className={`msg ${m.own ? "own" : ""} ${m.system ? "system" : ""}`}>
            {!m.system && (
              <span className="meta">
                {m.own ? t.you : m.displayName} · {time.format(m.sentAt)}
              </span>
            )}
            <span className="body">{m.text}</span>
          </div>
        ))}
      </div>
      <div className="typing">{typingNames.length > 0 ? t.typing(typingNames.join(", ")) : ""}</div>
      <form className="compose" onSubmit={(e) => void send(e)}>
        <input
          className="field"
          value={text}
          maxLength={1000}
          placeholder={t.chatPlaceholder}
          autoComplete="off"
          enterKeyHint="send"
          onChange={(e) => {
            setText(e.target.value);
            const now = performance.now();
            if (now - lastTyping.current > 1500) {
              lastTyping.current = now;
              controller.typing();
            }
          }}
        />
        <button type="submit" className="iconbtn" aria-label={t.send} title={t.send} disabled={!text.trim()}>
          <Icon name="send" />
        </button>
      </form>
    </aside>
  );
}
