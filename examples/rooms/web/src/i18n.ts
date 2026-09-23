import { createContext, useContext } from "react";

export type Lang = "en" | "ru";

const en = {
  appName: "Aurix Rooms",
  tagline: "Voice rooms for your squad. No accounts, no installs.",
  yourName: "Your name",
  namePlaceholder: "How should others see you?",
  micCheck: "Microphone",
  micIdle: "Tap to test your microphone",
  micOk: "Speak — the bar should move",
  micDenied: "Microphone access denied. Allow it in the browser settings.",
  micNone: "No microphone found",
  stage: "Stage",
  stageHint: "A bot plays music, speech and test signals around the clock — come and listen.",
  listening: (n: number) => (n === 1 ? "1 listening" : `${n} listening`),
  inRoom: (n: number) => (n === 1 ? "1 in room" : `${n} in room`),
  nowPlaying: "Now playing",
  botOffline: "The bot is taking a break",
  enter: "Enter",
  newRoom: "New room",
  roomTitle: "Room name (optional)",
  create: "Create",
  creating: "Creating…",
  haveCode: "Have a code?",
  codePlaceholder: "quiet-fox-417",
  join: "Join",
  joining: "Joining…",
  roomNotFound: "No such room",
  connecting: "Connecting…",
  reconnecting: "Reconnecting…",
  connected: "Connected",
  failed: "Connection failed",
  retry: "Retry",
  leave: "Leave",
  mute: "Mute",
  unmute: "Unmute",
  speakerOn: "Sound on",
  speakerOff: "Sound off",
  chat: "Chat",
  people: "People",
  send: "Send",
  chatPlaceholder: "Message",
  typing: (names: string) => `${names} typing…`,
  copyLink: "Copy link",
  copied: "Copied",
  you: "you",
  bot: "bot",
  muted: "muted",
  quality: "Quality",
  qualityLabels: ["", "Poor", "Weak", "Fair", "Good", "Excellent"] as readonly string[],
  rtt: "RTT",
  loss: "loss",
  jitter: "jitter",
  transport: { webrtc: "WebRTC", webtransport: "WebTransport", websocket: "WebSocket" } as Record<"webrtc" | "webtransport" | "websocket", string>,
  tapToHear: "Tap to enable sound",
  leftRoom: "You left the room",
  back: "Back to lobby",
  kicked: "You were removed from the room",
  kinds: { music: "Music", speech: "Speech", ambience: "Ambience", signal: "Test signal" } as Record<"music" | "speech" | "ambience" | "signal", string>,
  upNext: "Up next",
  stereo: "stereo",
  mono: "mono",
  poweredBy: "Runs on Aurix, an open self-hosted voice platform for games.",
  source: "Source",
  noMicWarn: "No microphone — you will only listen.",
  browserUnsupported: "This browser cannot run the voice engine. Try a recent Chrome, Edge, Firefox or Safari.",
  errorGeneric: "Something went wrong",
  rateLimited: "Too many attempts — wait a minute",
  empty: "Nobody else is here yet. Share the link.",
  license: "License",
};

const ru: typeof en = {
  appName: "Aurix Rooms",
  tagline: "Голосовые комнаты для своих. Без аккаунтов и установок.",
  yourName: "Ваше имя",
  namePlaceholder: "Как вас видеть остальным?",
  micCheck: "Микрофон",
  micIdle: "Нажмите, чтобы проверить микрофон",
  micOk: "Скажите что-нибудь — полоска должна двигаться",
  micDenied: "Доступ к микрофону запрещён. Разрешите его в настройках браузера.",
  micNone: "Микрофон не найден",
  stage: "Сцена",
  stageHint: "Бот круглосуточно крутит музыку, речь и тестовые сигналы — заходите послушать.",
  listening: (n: number) => `${n} ${plural(n, "слушает", "слушают", "слушают")}`,
  inRoom: (n: number) => `${n} в комнате`,
  nowPlaying: "Сейчас играет",
  botOffline: "Бот на перерыве",
  enter: "Войти",
  newRoom: "Новая комната",
  roomTitle: "Название (необязательно)",
  create: "Создать",
  creating: "Создаём…",
  haveCode: "Есть код?",
  codePlaceholder: "quiet-fox-417",
  join: "Войти",
  joining: "Входим…",
  roomNotFound: "Такой комнаты нет",
  connecting: "Подключение…",
  reconnecting: "Переподключение…",
  connected: "Подключено",
  failed: "Не удалось подключиться",
  retry: "Повторить",
  leave: "Выйти",
  mute: "Выключить микрофон",
  unmute: "Включить микрофон",
  speakerOn: "Звук включён",
  speakerOff: "Звук выключен",
  chat: "Чат",
  people: "Участники",
  send: "Отправить",
  chatPlaceholder: "Сообщение",
  typing: (names: string) => `${names} печатает…`,
  copyLink: "Скопировать ссылку",
  copied: "Скопировано",
  you: "вы",
  bot: "бот",
  muted: "без звука",
  quality: "Качество",
  qualityLabels: ["", "Плохо", "Слабо", "Средне", "Хорошо", "Отлично"] as readonly string[],
  rtt: "RTT",
  loss: "потери",
  jitter: "джиттер",
  transport: { webrtc: "WebRTC", webtransport: "WebTransport", websocket: "WebSocket" } as Record<"webrtc" | "webtransport" | "websocket", string>,
  tapToHear: "Нажмите, чтобы включить звук",
  leftRoom: "Вы вышли из комнаты",
  back: "В лобби",
  kicked: "Вас удалили из комнаты",
  kinds: { music: "Музыка", speech: "Речь", ambience: "Атмосфера", signal: "Тестовый сигнал" } as Record<"music" | "speech" | "ambience" | "signal", string>,
  upNext: "Далее",
  stereo: "стерео",
  mono: "моно",
  poweredBy: "Работает на Aurix — открытой self-hosted голосовой платформе для игр.",
  source: "Исходники",
  noMicWarn: "Микрофона нет — вы будете только слушать.",
  browserUnsupported: "Этот браузер не может запустить голосовой движок. Попробуйте свежий Chrome, Edge, Firefox или Safari.",
  errorGeneric: "Что-то пошло не так",
  rateLimited: "Слишком много попыток — подождите минуту",
  empty: "Пока никого. Поделитесь ссылкой.",
  license: "Лицензия",
};

function plural(n: number, one: string, few: string, many: string): string {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return one;
  if (m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14)) return few;
  return many;
}

export type Strings = typeof en;
export const STRINGS: Record<Lang, Strings> = { en, ru };

const LANG_KEY = "rooms.lang";

export function detectLang(): Lang {
  const saved = localStorage.getItem(LANG_KEY);
  if (saved === "en" || saved === "ru") return saved;
  return navigator.language.toLowerCase().startsWith("ru") ? "ru" : "en";
}

export function saveLang(lang: Lang): void {
  localStorage.setItem(LANG_KEY, lang);
  document.documentElement.lang = lang;
}

export const I18nContext = createContext<{ lang: Lang; t: Strings; setLang: (l: Lang) => void }>({
  lang: "en",
  t: en,
  setLang: () => undefined,
});

export function useT() {
  return useContext(I18nContext);
}
