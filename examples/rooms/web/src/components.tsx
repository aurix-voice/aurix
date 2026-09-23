import type { ReactNode } from "react";

import { hueFor, initials } from "./identity";
import { useT } from "./i18n";

const PATHS = {
  mic: "M12 15a4 4 0 0 0 4-4V6a4 4 0 1 0-8 0v5a4 4 0 0 0 4 4Zm7-4a7 7 0 0 1-6 6.93V21h-2v-3.07A7 7 0 0 1 5 11h2a5 5 0 0 0 10 0h2Z",
  micOff:
    "M4.3 3 3 4.3 8 9.3V11a4 4 0 0 0 5.5 3.7l1.5 1.5A6.9 6.9 0 0 1 12 17a7 7 0 0 1-7-6H3a9 9 0 0 0 8 8.9V21h2v-1.1a8.9 8.9 0 0 0 3.5-1.2l3.2 3.3 1.3-1.3L4.3 3Zm11.7 8V6a4 4 0 0 0-8-.4l8 8c0-.2 0-.4 0-.6Zm3 0h-2c0 .6-.1 1.2-.3 1.8l1.5 1.5c.5-1 .8-2.1.8-3.3Z",
  speaker: "M4 9v6h4l5 4V5L8 9H4Zm12.5 3a4.5 4.5 0 0 0-2.5-4v8a4.5 4.5 0 0 0 2.5-4Zm-2.5-8v2a6 6 0 0 1 0 12v2a8 8 0 0 0 0-16Z",
  speakerOff:
    "M4 9v6h4l5 4V5L8 9H4Zm12.6 3 2.6-2.6-1.3-1.3-2.6 2.6-2.6-2.6-1.3 1.3 2.6 2.6-2.6 2.6 1.3 1.3 2.6-2.6 2.6 2.6 1.3-1.3L16.6 12Z",
  chat: "M4 4h16v12H7l-3 3V4Zm2 2v8.2L6.2 14H18V6H6Z",
  leave: "M10 4H4v16h6v-2H6V6h4V4Zm5 4-1.4 1.4L15.2 11H9v2h6.2l-1.6 1.6L15 16l4-4-4-4Z",
  link: "M10.6 13.4a1 1 0 0 0 1.4 0l3-3a3 3 0 0 0-4.2-4.2l-1.2 1.2 1.4 1.4 1.2-1.2a1 1 0 0 1 1.4 1.4l-3 3a1 1 0 0 1-1.4 0l-1.4 1.4Zm2.8-2.8a1 1 0 0 0-1.4 0l-3 3a3 3 0 0 0 4.2 4.2l1.2-1.2-1.4-1.4-1.2 1.2a1 1 0 0 1-1.4-1.4l3-3a1 1 0 0 1 1.4 0l1.4-1.4Z",
  send: "M3 11 21 3l-8 18-2-7-8-3Zm5.7 1.2 3.1 1.2.9 3.2 4.8-10.8L8.7 12.2Z",
  back: "M15 5l-7 7 7 7 1.4-1.4L10.8 12l5.6-5.6L15 5Z",
  music: "M14 3v10.6a3.5 3.5 0 1 0 2 3.2V7h4V3h-6Zm-8 5v7.6A3.5 3.5 0 1 0 8 18.8V8H6Z",
  speech: "M12 3a4 4 0 0 1 4 4v4a4 4 0 0 1-8 0V7a4 4 0 0 1 4-4Zm-6 8h2a4 4 0 0 0 8 0h2a6 6 0 0 1-5 5.9V19h3v2H8v-2h3v-2.1A6 6 0 0 1 6 11Z",
  ambience: "M4 15h2a6 6 0 0 1 12 0h2a8 8 0 0 0-16 0Zm4 0h2a2 2 0 0 1 4 0h2a4 4 0 0 0-8 0ZM3 18h18v2H3z",
  signal: "M3 12h3l2-6 3 12 3-9 2 5 1-2h4v2h-3l-2 4-2-5-3 9-3-12-1 3H3z",
  check: "M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4L9 16.2Z",
  close: "M18.3 5.7 12 12l6.3 6.3-1.4 1.4L10.6 13.4 4.3 19.7 2.9 18.3 9.2 12 2.9 5.7l1.4-1.4 6.3 6.3 6.3-6.3 1.4 1.4Z",
  people: "M16 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-8 0a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 2c-2.7 0-6 1.3-6 4v2h9v-2c0-1.2.5-2.2 1.3-3A8.8 8.8 0 0 0 8 13Zm8 0c-.6 0-1.2.1-1.8.2.9.9 1.4 2 1.4 3.3V19H22v-2c0-2.7-3.3-4-6-4Z",
};

export type IconName = keyof typeof PATHS;

export function Icon({ name, size }: { name: IconName; size?: number }) {
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} fill="currentColor" aria-hidden="true">
      <path d={PATHS[name]} />
    </svg>
  );
}

export function IconButton({
  icon,
  label,
  onClick,
  on = false,
  danger = false,
  dot = false,
  disabled = false,
  className = "",
}: {
  icon: IconName;
  label: string;
  onClick: () => void;
  on?: boolean;
  danger?: boolean;
  dot?: boolean;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <button
      type="button"
      className={`iconbtn ${on ? "on" : ""} ${danger ? "danger" : ""} ${className}`}
      onClick={onClick}
      aria-label={label}
      aria-pressed={on || undefined}
      title={label}
      disabled={disabled}
    >
      <Icon name={icon} />
      {dot && <span className="dot" />}
    </button>
  );
}

export function Avatar({ id, name, bot = false }: { id: string; name: string; bot?: boolean }) {
  const hue = hueFor(id);
  const style = bot
    ? { background: "var(--fg)", color: "var(--bg)" }
    : { background: `hsl(${hue} 22% 46%)` };
  return (
    <span className="avatar" style={style} aria-hidden="true">
      {bot ? <Icon name="music" size={22} /> : initials(name)}
    </span>
  );
}

export function Logo() {
  return (
    <svg viewBox="0 0 32 32" aria-hidden="true">
      <rect width="32" height="32" rx="8" fill="var(--fg)" />
      <path d="M9 16v0M13 11v10M17 8v16M21 12v8M25 15v2" stroke="var(--bg)" strokeWidth="2.4" strokeLinecap="round" />
    </svg>
  );
}

/** Network quality: 1..5 bars, coloured only when it is worth noticing. */
export function Bars({ bars, title }: { bars: number | undefined; title: string }) {
  const n = bars ?? 0;
  return (
    <span className={`bars q${n}`} title={title} aria-label={title} role="img">
      {[1, 2, 3, 4, 5].map((i) => (
        <i key={i} className={i <= n ? "on" : ""} style={{ height: `${4 + i * 2}px` }} />
      ))}
    </span>
  );
}

export function Equalizer() {
  return (
    <span className="eq" aria-hidden="true">
      <i />
      <i />
      <i />
      <i />
    </span>
  );
}

export function Center({ title, children }: { title?: string; children?: ReactNode }) {
  return (
    <div className="center">
      {title && <h2>{title}</h2>}
      {children}
    </div>
  );
}

export function LangToggle() {
  const { lang, setLang } = useT();
  const next = lang === "ru" ? "en" : "ru";
  return (
    <button type="button" className="lang" onClick={() => setLang(next)} aria-label={next === "ru" ? "Русский" : "English"}>
      {next === "ru" ? "RU" : "EN"}
    </button>
  );
}
