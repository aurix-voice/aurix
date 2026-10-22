/**
 * Push-to-talk for browsers: hold (or toggle) a key while the microphone is unmuted; everything
 * else stays muted. Drives `client.setMuted`, so the server, the roster flags and `isMuted()` agree.
 */
export interface PushToTalkOptions {
  /** `KeyboardEvent.code` values that transmit, e.g. `["KeyV"]` or `["Space"]`. */
  keys: string[];
  /** Press once to start, press again to stop (instead of hold). Default `false`. */
  toggle?: boolean;
  /** Keep transmitting this long after release so the last word is not cut (ms). Default 150. */
  releaseDelayMs?: number;
  /** Ignore key presses while an editable element has focus. Default `true`. */
  ignoreWhenTyping?: boolean;
  /** Where to listen; defaults to `window`. */
  target?: Pick<EventTarget, 'addEventListener' | 'removeEventListener'>;
  /** Called when transmission starts/stops (for a UI indicator). */
  onChange?: (transmitting: boolean) => void;
}

interface MuteControl {
  setMuted(muted: boolean): void;
  isMuted(): boolean;
}

function typing(ev: KeyboardEvent): boolean {
  const el = ev.target as HTMLElement | null;
  if (!el || typeof el.tagName !== 'string') return false;
  const tag = el.tagName.toLowerCase();
  return tag === 'input' || tag === 'textarea' || tag === 'select' || el.isContentEditable === true;
}

/**
 * Bind push-to-talk to a client. Returns a handle with `dispose()` (unbinds and mutes) and
 * `transmitting`. The client starts muted when PTT is attached.
 */
export function attachPushToTalk(client: MuteControl, opts: PushToTalkOptions) {
  const keys = new Set(opts.keys);
  const toggle = opts.toggle ?? false;
  const releaseDelay = opts.releaseDelayMs ?? 150;
  const ignoreTyping = opts.ignoreWhenTyping ?? true;
  const target = opts.target ?? (typeof window !== 'undefined' ? window : undefined);
  let transmitting = false;
  let releaseTimer: ReturnType<typeof setTimeout> | null = null;
  const held = new Set<string>();

  const set = (on: boolean) => {
    if (transmitting === on) return;
    transmitting = on;
    if (client.isMuted() === on) client.setMuted(!on);
    opts.onChange?.(on);
  };
  // Toggle latches on the press edge; hold mirrors the key. The latch is separate from
  // `transmitting` so a toggle-off runs out the release delay like the engine SDKs do.
  let latched = false;
  const scheduleStop = () => {
    if (releaseTimer) clearTimeout(releaseTimer);
    releaseTimer = setTimeout(() => {
      releaseTimer = null;
      if (!latched && (toggle || held.size === 0)) set(false);
    }, releaseDelay);
  };
  const open = () => {
    latched = true;
    if (releaseTimer) {
      clearTimeout(releaseTimer);
      releaseTimer = null;
    }
    set(true);
  };
  const close = () => {
    latched = false;
    if (releaseDelay <= 0) set(false);
    else scheduleStop();
  };
  const flip = () => (latched ? close() : open());
  const onDown = (e: Event) => {
    const ev = e as KeyboardEvent;
    if (!keys.has(ev.code) || (ignoreTyping && typing(ev))) return;
    if (ev.repeat) return;
    held.add(ev.code);
    if (toggle) flip();
    else open();
  };
  const onUp = (e: Event) => {
    const ev = e as KeyboardEvent;
    if (!keys.has(ev.code)) return;
    held.delete(ev.code);
    if (!toggle && held.size === 0) close();
  };
  const onBlur = () => {
    held.clear();
    if (!toggle) close();
  };

  if (!client.isMuted()) client.setMuted(true);
  target?.addEventListener('keydown', onDown);
  target?.addEventListener('keyup', onUp);
  target?.addEventListener('blur', onBlur);

  return {
    get transmitting() {
      return transmitting;
    },
    /** Programmatic press/release — wire a gamepad button or an on-screen button to it. */
    press(down: boolean) {
      if (toggle) {
        if (down) flip();
      } else if (down) open();
      else close();
    },
    dispose() {
      target?.removeEventListener('keydown', onDown);
      target?.removeEventListener('keyup', onUp);
      target?.removeEventListener('blur', onBlur);
      if (releaseTimer) clearTimeout(releaseTimer);
      set(false);
    },
  };
}
