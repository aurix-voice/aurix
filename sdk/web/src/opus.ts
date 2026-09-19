/**
 * Opus controls for the browser.
 *
 * A browser never exposes the encoder directly: everything goes through WebRTC. What *is*
 * controllable, and how this module maps the server's channel policy onto it:
 *
 * | Control                     | Mechanism                                                      | Live? |
 * |-----------------------------|----------------------------------------------------------------|-------|
 * | bitrate (target / ceiling)  | `RTCRtpSender.setParameters({ encodings: [{ maxBitrate }] })`  | yes   |
 * | bitrate (ceiling)           | `maxaveragebitrate` in the remote answer's Opus `fmtp`         | no    |
 * | in-band FEC                 | `useinbandfec` in the remote answer's Opus `fmtp` (RFC 7587)   | no    |
 * | DTX                         | `usedtx` in the remote answer's Opus `fmtp`                    | no    |
 * | max bandwidth               | `maxplaybackrate` in the remote answer's Opus `fmtp`           | no    |
 * | constant bitrate            | `cbr` in the remote answer's Opus `fmtp`                       | no    |
 * | complexity, signal, VBR, expected loss | **not controllable** — the browser owns them        | —     |
 *
 * RFC 7587 defines the `fmtp` parameters as the *receiver's* preferences, which the sender (the
 * browser, for our uplink) honours; the SDK therefore rewrites the Opus `fmtp` line of the
 * server's answer before `setRemoteDescription`. Those take effect at negotiation only, so a
 * policy that arrives later changes the live bitrate right away and the rest on the next media
 * negotiation (`AurixClient.renegotiateMedia()` or a reconnect). Browsers also keep their own
 * congestion control underneath `maxBitrate`; the SDK never disables it.
 */

import type { AudioPolicyWire } from './protocol.js';

export type OpusBandwidth = 'narrowband' | 'mediumband' | 'wideband' | 'superwideband' | 'fullband';
export type OpusSignal = 'auto' | 'voice' | 'music';

const BANDWIDTH_ORDER: readonly OpusBandwidth[] = [
  'narrowband',
  'mediumband',
  'wideband',
  'superwideband',
  'fullband',
];

/** Server-side per-channel audio policy (`ChannelJoinAck.audio` / `ChannelAudioPolicy`). */
export interface AudioPolicy {
  /** Target bitrate the channel is tuned for (bit/s). */
  bitrateBps: number;
  /** Floor the server's adaptive bitrate never goes below (bit/s). */
  minBitrateBps: number;
  /** The channel wants in-band FEC on uplinks. */
  fec: boolean;
  /** The channel allows DTX (silence suppression) on uplinks. */
  dtx: boolean;
  /** Widest bandwidth the channel needs. */
  maxBandwidth: OpusBandwidth;
  /** Encoder complexity hint (0–10); browsers cannot honour it (kept for parity with native SDKs). */
  complexity?: number;
  /** Content hint; browsers cannot honour it. */
  signal: OpusSignal;
}

/** The server's default channel policy. */
export const DEFAULT_AUDIO_POLICY: AudioPolicy = Object.freeze({
  bitrateBps: 48_000,
  minBitrateBps: 12_000,
  fec: true,
  dtx: true,
  maxBandwidth: 'fullband',
  signal: 'voice',
});

export function isOpusBandwidth(v: unknown): v is OpusBandwidth {
  return typeof v === 'string' && (BANDWIDTH_ORDER as readonly string[]).includes(v);
}

export function isOpusSignal(v: unknown): v is OpusSignal {
  return v === 'auto' || v === 'voice' || v === 'music';
}

/** Sample rate that covers `bandwidth` (RFC 7587 `maxplaybackrate` values). */
export function opusPlaybackRateHz(bandwidth: OpusBandwidth): number {
  switch (bandwidth) {
    case 'narrowband':
      return 8_000;
    case 'mediumband':
      return 12_000;
    case 'wideband':
      return 16_000;
    case 'superwideband':
      return 24_000;
    case 'fullband':
      return 48_000;
  }
}

function widerBandwidth(a: OpusBandwidth, b: OpusBandwidth): OpusBandwidth {
  return BANDWIDTH_ORDER.indexOf(a) >= BANDWIDTH_ORDER.indexOf(b) ? a : b;
}

/** Parse the wire object; missing keys take the server defaults, `complexity: null` means no hint. */
export function parseAudioPolicy(wire: AudioPolicyWire | undefined | null): AudioPolicy {
  const d = DEFAULT_AUDIO_POLICY;
  if (!wire || typeof wire !== 'object') return { ...d };
  const num = (v: unknown, fallback: number): number =>
    typeof v === 'number' && Number.isFinite(v) ? v : fallback;
  const policy: AudioPolicy = {
    bitrateBps: num(wire.bitrate_bps, d.bitrateBps),
    minBitrateBps: num(wire.min_bitrate_bps, d.minBitrateBps),
    fec: typeof wire.fec === 'boolean' ? wire.fec : d.fec,
    dtx: typeof wire.dtx === 'boolean' ? wire.dtx : d.dtx,
    maxBandwidth: isOpusBandwidth(wire.max_bandwidth) ? wire.max_bandwidth : d.maxBandwidth,
    signal: isOpusSignal(wire.signal) ? wire.signal : d.signal,
  };
  if (typeof wire.complexity === 'number' && Number.isFinite(wire.complexity)) {
    policy.complexity = Math.min(10, Math.max(0, Math.round(wire.complexity)));
  }
  return policy;
}

/**
 * Combined policy for one uplink feeding several channels, with the same semantics as the
 * native and Unity SDKs: the widest bitrate and bandwidth so no channel is starved, FEC if any
 * channel wants it, DTX only if every channel allows it, the highest complexity hint, and
 * `music` over `voice` over `auto`.
 */
export function mergeAudioPolicies(a: AudioPolicy, b: AudioPolicy): AudioPolicy {
  const merged: AudioPolicy = {
    bitrateBps: Math.max(a.bitrateBps, b.bitrateBps),
    minBitrateBps: Math.max(a.minBitrateBps, b.minBitrateBps),
    fec: a.fec || b.fec,
    dtx: a.dtx && b.dtx,
    maxBandwidth: widerBandwidth(a.maxBandwidth, b.maxBandwidth),
    signal:
      a.signal === 'music' || b.signal === 'music'
        ? 'music'
        : a.signal === 'voice' || b.signal === 'voice'
          ? 'voice'
          : 'auto',
  };
  if (a.complexity !== undefined && b.complexity !== undefined) {
    merged.complexity = Math.max(a.complexity, b.complexity);
  } else if (a.complexity !== undefined) {
    merged.complexity = a.complexity;
  } else if (b.complexity !== undefined) {
    merged.complexity = b.complexity;
  }
  return merged;
}

/** Merge of all policies; `DEFAULT_AUDIO_POLICY` when there are none. */
export function mergeAllAudioPolicies(policies: Iterable<AudioPolicy>): AudioPolicy {
  let acc: AudioPolicy | undefined;
  for (const p of policies) acc = acc ? mergeAudioPolicies(acc, p) : p;
  return acc ?? { ...DEFAULT_AUDIO_POLICY };
}

export function audioPoliciesEqual(a: AudioPolicy | undefined, b: AudioPolicy | undefined): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.bitrateBps === b.bitrateBps &&
    a.minBitrateBps === b.minBitrateBps &&
    a.fec === b.fec &&
    a.dtx === b.dtx &&
    a.maxBandwidth === b.maxBandwidth &&
    a.complexity === b.complexity &&
    a.signal === b.signal
  );
}

/** Local Opus preferences for the browser uplink (`AurixClientOptions.opus`). */
export interface OpusBrowserOptions {
  /**
   * Uplink bitrate ceiling (bit/s) applied through `RTCRtpSender.setParameters` and
   * `maxaveragebitrate`. Unset: the channel policy's target (or the browser default when
   * `followChannelPolicy` is off).
   */
  maxBitrateBps?: number;
  /** Ask the browser for in-band FEC (`useinbandfec=1`). Unset: channel policy (default on). */
  fec?: boolean;
  /** Allow DTX (`usedtx=1`). Unset: channel policy (default on). */
  dtx?: boolean;
  /** Cap the encoded bandwidth (`maxplaybackrate`). Unset: channel policy (default fullband). */
  maxBandwidth?: OpusBandwidth;
  /** Constant bitrate (`cbr=1`); default off (VBR). */
  cbr?: boolean;
  /**
   * Follow the server's channel audio policy for everything not pinned above (default `true`).
   * With `false` only the explicit options are applied and the browser defaults do the rest.
   */
  followChannelPolicy?: boolean;
}

/** What the SDK asks the browser's Opus encoder for (the resolved options + policy). */
export interface OpusSenderPreferences {
  maxBitrateBps?: number;
  fec?: boolean;
  dtx?: boolean;
  maxPlaybackRateHz?: number;
  cbr?: boolean;
}

/** Resolve local options over the (merged) channel policy into concrete sender preferences. */
export function resolveOpusSenderPreferences(
  opts: OpusBrowserOptions | undefined,
  policy: AudioPolicy | undefined,
  transientBitrateBps?: number,
): OpusSenderPreferences {
  const o = opts ?? {};
  const follow = o.followChannelPolicy !== false;
  const p = follow ? policy : undefined;
  const prefs: OpusSenderPreferences = {};
  const ceiling = o.maxBitrateBps ?? p?.bitrateBps;
  const bitrate =
    transientBitrateBps !== undefined
      ? ceiling !== undefined
        ? Math.min(transientBitrateBps, ceiling)
        : transientBitrateBps
      : ceiling;
  if (bitrate !== undefined) prefs.maxBitrateBps = Math.max(6_000, Math.round(bitrate));
  const fec = o.fec ?? p?.fec;
  if (fec !== undefined) prefs.fec = fec;
  const dtx = o.dtx ?? p?.dtx;
  if (dtx !== undefined) prefs.dtx = dtx;
  const bw = o.maxBandwidth ?? p?.maxBandwidth;
  if (bw !== undefined) prefs.maxPlaybackRateHz = opusPlaybackRateHz(bw);
  if (o.cbr !== undefined) prefs.cbr = o.cbr;
  return prefs;
}

export function senderPreferencesEqual(a: OpusSenderPreferences, b: OpusSenderPreferences): boolean {
  return (
    a.maxBitrateBps === b.maxBitrateBps &&
    a.fec === b.fec &&
    a.dtx === b.dtx &&
    a.maxPlaybackRateHz === b.maxPlaybackRateHz &&
    a.cbr === b.cbr
  );
}

/**
 * Rewrite the Opus `a=fmtp:` lines of an SDP (the server's answer, i.e. what *we* receive and
 * therefore what the browser's encoder honours) with `prefs`. Parameters not covered by `prefs`
 * (e.g. `sprop-stereo`, `minptime`) are preserved; `stereo=`, if present, is left untouched.
 */
export function applyOpusSenderPreferences(sdp: string, prefs: OpusSenderPreferences): string {
  const overrides = new Map<string, string | undefined>();
  if (prefs.fec !== undefined) overrides.set('useinbandfec', prefs.fec ? '1' : '0');
  if (prefs.dtx !== undefined) overrides.set('usedtx', prefs.dtx ? '1' : '0');
  if (prefs.maxBitrateBps !== undefined) overrides.set('maxaveragebitrate', String(prefs.maxBitrateBps));
  if (prefs.maxPlaybackRateHz !== undefined) overrides.set('maxplaybackrate', String(prefs.maxPlaybackRateHz));
  if (prefs.cbr !== undefined) overrides.set('cbr', prefs.cbr ? '1' : '0');
  if (overrides.size === 0) return sdp;

  const lines = sdp.split(/\r?\n/);
  const opusPts = new Set<string>();
  for (const line of lines) {
    const m = /^a=rtpmap:(\d+) opus\/48000(?:\/2)?/i.exec(line);
    if (m?.[1] !== undefined) opusPts.add(m[1]);
  }
  if (opusPts.size === 0) return sdp;

  const rewrite = (existing: string): string => {
    const kept = existing
      .split(';')
      .map((p) => p.trim())
      .filter((p) => p.length > 0)
      .filter((p) => {
        const key = p.split('=')[0]?.trim().toLowerCase();
        return key === undefined || !overrides.has(key);
      });
    for (const [k, v] of overrides) if (v !== undefined) kept.push(`${k}=${v}`);
    return kept.join(';');
  };

  const seen = new Set<string>();
  const out: string[] = [];
  for (const line of lines) {
    const m = /^a=fmtp:(\d+) (.*)$/.exec(line);
    if (m?.[1] !== undefined && m[2] !== undefined && opusPts.has(m[1])) {
      seen.add(m[1]);
      out.push(`a=fmtp:${m[1]} ${rewrite(m[2])}`);
      continue;
    }
    out.push(line);
  }
  const result: string[] = [];
  for (const line of out) {
    result.push(line);
    const m = /^a=rtpmap:(\d+) opus\/48000(?:\/2)?/i.exec(line);
    if (m?.[1] !== undefined && !seen.has(m[1])) result.push(`a=fmtp:${m[1]} ${rewrite('')}`);
  }
  return result.join(sdp.includes('\r\n') ? '\r\n' : '\n');
}

/** Parse the Opus `fmtp` parameters of an SDP into sender preferences (what was negotiated). */
export function negotiatedOpusPreferences(sdp: string | undefined): OpusSenderPreferences {
  const prefs: OpusSenderPreferences = {};
  if (!sdp) return prefs;
  const lines = sdp.split(/\r?\n/);
  const opusPts = new Set<string>();
  for (const line of lines) {
    const m = /^a=rtpmap:(\d+) opus\/48000(?:\/2)?/i.exec(line);
    if (m?.[1] !== undefined) opusPts.add(m[1]);
  }
  for (const line of lines) {
    const m = /^a=fmtp:(\d+) (.*)$/.exec(line);
    if (m?.[1] === undefined || m[2] === undefined || !opusPts.has(m[1])) continue;
    for (const param of m[2].split(';')) {
      const [k, v] = param.split('=').map((s) => s.trim());
      if (k === undefined || v === undefined) continue;
      switch (k.toLowerCase()) {
        case 'useinbandfec':
          prefs.fec = v === '1';
          break;
        case 'usedtx':
          prefs.dtx = v === '1';
          break;
        case 'maxaveragebitrate':
          prefs.maxBitrateBps = Number(v);
          break;
        case 'maxplaybackrate':
          prefs.maxPlaybackRateHz = Number(v);
          break;
        case 'cbr':
          prefs.cbr = v === '1';
          break;
        default:
          break;
      }
    }
    break;
  }
  return prefs;
}
