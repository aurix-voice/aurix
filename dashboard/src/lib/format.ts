import type { Locale } from "@/i18n";

const bcp47: Record<Locale, string> = { en: "en-US", ru: "ru-RU" };

export function fmtNumber(locale: Locale, n: number | null | undefined, digits = 0): string {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  return new Intl.NumberFormat(bcp47[locale], { maximumFractionDigits: digits }).format(n);
}

export function fmtCompact(locale: Locale, n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  return new Intl.NumberFormat(bcp47[locale], { notation: "compact", maximumFractionDigits: 1 }).format(n);
}

export function fmtPercent(locale: Locale, n: number | null | undefined, digits = 1): string {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  return `${new Intl.NumberFormat(bcp47[locale], { maximumFractionDigits: digits }).format(n)} %`;
}

export function fmtBytes(locale: Locale, n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${new Intl.NumberFormat(bcp47[locale], { maximumFractionDigits: i === 0 ? 0 : 1 }).format(v)} ${units[i]}`;
}

export function fmtDateTime(locale: Locale, iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(bcp47[locale], {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(d);
}

/** Compact "Sep 21, 18:43:51" for dense log tables; pair with the full `fmtDateTime` in a title. */
export function fmtDateTimeShort(locale: Locale, iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(bcp47[locale], {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  }).format(d);
}

export function fmtDate(locale: Locale, iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(bcp47[locale], { year: "numeric", month: "short", day: "2-digit" }).format(d);
}

export function fmtTime(locale: Locale, iso: string | number | null | undefined): string {
  if (iso === null || iso === undefined) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return String(iso);
  return new Intl.DateTimeFormat(bcp47[locale], { hour: "2-digit", minute: "2-digit" }).format(d);
}

/** "3 min ago" / "3 мин назад" style; `now` is injectable for tests. */
export function fmtRelative(locale: Locale, iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "—";
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return iso;
  const rtf = new Intl.RelativeTimeFormat(bcp47[locale], { numeric: "auto", style: "narrow" });
  const diff = (t - now) / 1000;
  const abs = Math.abs(diff);
  if (abs < 45) return rtf.format(Math.round(diff), "second");
  if (abs < 3600) return rtf.format(Math.round(diff / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), "hour");
  return rtf.format(Math.round(diff / 86400), "day");
}

export function fmtDuration(locale: Locale, secs: number | null | undefined): string {
  if (secs === null || secs === undefined || Number.isNaN(secs)) return "—";
  const s = Math.max(0, Math.round(secs));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  const nf = new Intl.NumberFormat(bcp47[locale]);
  if (h > 0) return `${nf.format(h)}:${pad(m)}:${pad(r)}`;
  return `${m}:${pad(r)}`;
}

export function fmtMinutes(locale: Locale, minutes: number | null | undefined): string {
  if (minutes === null || minutes === undefined || Number.isNaN(minutes)) return "—";
  if (minutes < 60) return `${fmtNumber(locale, minutes)} min`;
  return `${fmtNumber(locale, minutes / 60, 1)} h`;
}

export function fmtMos(n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  return n.toFixed(2);
}

export function shortId(id: string | null | undefined, n = 8): string {
  if (!id) return "—";
  return id.length > n ? id.slice(0, n) : id;
}

function pad(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

export function toIsoStart(date: string): string {
  return new Date(`${date}T00:00:00`).toISOString();
}

export function toIsoEnd(date: string): string {
  return new Date(`${date}T23:59:59.999`).toISOString();
}

export function isoDay(d: Date): string {
  const y = d.getFullYear();
  const m = pad(d.getMonth() + 1);
  const day = pad(d.getDate());
  return `${y}-${m}-${day}`;
}
