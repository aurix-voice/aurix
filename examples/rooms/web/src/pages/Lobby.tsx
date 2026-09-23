import { AudioLevelMeter } from "@aurix/web-sdk";
import { useEffect, useRef, useState, type FormEvent } from "react";

import { ApiError, api, subscribeBot, type BotStatus, type RoomCard } from "../api";
import { Equalizer, Icon, LangToggle, Logo } from "../components";
import { saveName, savedName } from "../identity";
import { useT } from "../i18n";
import { navigate, roomPath } from "../router";

const SOURCE_URL = "https://github.com/aurix-voice/aurix";

type MicState = "idle" | "asking" | "live" | "denied" | "none";

function useMicCheck() {
  const [state, setState] = useState<MicState>("idle");
  const [level, setLevel] = useState(0);
  const meter = useRef<AudioLevelMeter>(undefined);
  const stream = useRef<MediaStream>(undefined);

  const stop = () => {
    meter.current?.stop();
    meter.current = undefined;
    stream.current?.getTracks().forEach((t) => t.stop());
    stream.current = undefined;
    setLevel(0);
  };

  const start = async () => {
    if (state === "live") {
      stop();
      setState("idle");
      return;
    }
    setState("asking");
    try {
      const s = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
      stream.current = s;
      meter.current = new AudioLevelMeter(s, (sample) => setLevel(sample.energy), { intervalMs: 50 });
      meter.current.start();
      setState("live");
      setTimeout(() => {
        if (meter.current) {
          stop();
          setState("idle");
        }
      }, 15_000);
    } catch (e) {
      setState(e instanceof DOMException && e.name === "NotFoundError" ? "none" : "denied");
    }
  };

  useEffect(() => stop, []);
  return { state, level, toggle: start };
}

function StageCard({ slug, name, disabled }: { slug: string; name: string; disabled: boolean }) {
  const { t } = useT();
  const [card, setCard] = useState<RoomCard>();
  const [bot, setBot] = useState<BotStatus | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      api
        .room(slug)
        .then((c) => alive && setCard(c))
        .catch(() => undefined);
    void load();
    const timer = setInterval(load, 15_000);
    const unsubscribe = subscribeBot(slug, setBot);
    return () => {
      alive = false;
      clearInterval(timer);
      unsubscribe();
    };
  }, [slug]);

  const listeners = Math.max(0, (card?.participants ?? 0) - (bot ? 1 : 0));
  return (
    <section className="card" aria-labelledby="stage-title">
      <div className="head">
        <h2 id="stage-title">{card?.title ?? t.stage}</h2>
        <span className={`count ${listeners === 0 ? "idle" : ""}`}>{t.listening(listeners)}</span>
      </div>
      <div className="now">
        <span className={`art ${bot ? "live" : ""}`}>{bot ? <Equalizer /> : <Icon name="music" size={20} />}</span>
        <div>
          <div className="title">{bot ? bot.track.title : t.botOffline}</div>
          <div className="sub">
            {bot
              ? [t.kinds[bot.track.kind], bot.track.artist, bot.track.stereo ? t.stereo : undefined].filter(Boolean).join(" · ")
              : t.stageHint}
          </div>
        </div>
      </div>
      <button
        type="button"
        className="btn block"
        disabled={disabled}
        onClick={() => {
          saveName(name.trim());
          navigate(roomPath(slug), { autoJoin: true });
        }}
      >
        {t.enter}
      </button>
    </section>
  );
}

export function Lobby({ stage, maxName }: { stage: string; maxName: number }) {
  const { t } = useT();
  const [name, setName] = useState(savedName);
  const [title, setTitle] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState<"create" | "join" | undefined>();
  const [error, setError] = useState<string>();
  const mic = useMicCheck();
  const nameOk = name.trim().length > 0 && name.trim().length <= maxName;

  const describe = (e: unknown) => {
    if (e instanceof ApiError && e.status === 429) return t.rateLimited;
    if (e instanceof ApiError && e.status === 404) return t.roomNotFound;
    return t.errorGeneric;
  };

  const create = async (ev: FormEvent) => {
    ev.preventDefault();
    if (!nameOk || busy) return;
    setBusy("create");
    setError(undefined);
    try {
      saveName(name.trim());
      const room = await api.createRoom(title.trim() || undefined);
      navigate(roomPath(room.slug), { autoJoin: true });
    } catch (e) {
      setError(describe(e));
      setBusy(undefined);
    }
  };

  const join = async (ev: FormEvent) => {
    ev.preventDefault();
    const slug = code
      .trim()
      .toLowerCase()
      .replace(/^.*\/r\//, "")
      .replace(/[^a-z0-9-]/g, "");
    if (!nameOk || !slug || busy) return;
    setBusy("join");
    setError(undefined);
    try {
      await api.room(slug);
      saveName(name.trim());
      navigate(roomPath(slug), { autoJoin: true });
    } catch (e) {
      setError(describe(e));
      setBusy(undefined);
    }
  };

  const micText = { idle: t.micIdle, asking: "…", live: t.micOk, denied: t.micDenied, none: t.micNone }[mic.state];

  return (
    <main className="page lobby">
      <header className="brand">
        <div>
          <h1>
            <Logo />
            {t.appName}
          </h1>
          <p className="tagline">{t.tagline}</p>
        </div>
        <LangToggle />
      </header>

      <section className="section">
        <label className="label" htmlFor="name">
          {t.yourName}
        </label>
        <input
          id="name"
          className="field"
          value={name}
          maxLength={maxName}
          autoComplete="nickname"
          placeholder={t.namePlaceholder}
          onChange={(e) => setName(e.target.value)}
        />
        <button type="button" className="mic" onClick={() => void mic.toggle()} aria-pressed={mic.state === "live"}>
          <Icon name={mic.state === "denied" || mic.state === "none" ? "micOff" : "mic"} size={20} />
          <span className="meter" aria-hidden="true">
            <i style={{ width: `${Math.min(100, Math.round(mic.level * 140))}%` }} />
          </span>
          <span className={`text ${mic.state === "denied" || mic.state === "none" ? "bad" : ""}`}>{micText}</span>
        </button>
      </section>

      <StageCard slug={stage} name={name} disabled={!nameOk} />

      <form className="section" onSubmit={(e) => void create(e)}>
        <span className="label">{t.newRoom}</span>
        <div className="row">
          <input
            className="field"
            value={title}
            maxLength={48}
            placeholder={t.roomTitle}
            onChange={(e) => setTitle(e.target.value)}
          />
          <button type="submit" className="btn" disabled={!nameOk || busy !== undefined}>
            {busy === "create" ? t.creating : t.create}
          </button>
        </div>
      </form>

      <form className="section" onSubmit={(e) => void join(e)}>
        <span className="label">{t.haveCode}</span>
        <div className="row">
          <input
            className="field"
            value={code}
            placeholder={t.codePlaceholder}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            inputMode="url"
            onChange={(e) => setCode(e.target.value)}
          />
          <button type="submit" className="btn ghost" disabled={!nameOk || !code.trim() || busy !== undefined}>
            {busy === "join" ? t.joining : t.join}
          </button>
        </div>
        {error && (
          <p className="hint bad" role="alert">
            {error}
          </p>
        )}
      </form>

      <footer className="footer">
        <span>{t.poweredBy}</span>
        <a href={SOURCE_URL} target="_blank" rel="noreferrer">
          {t.source}
        </a>
        <a href={`${SOURCE_URL}/blob/main/LICENSING.md`} target="_blank" rel="noreferrer">
          {t.license}
        </a>
      </footer>
    </main>
  );
}
