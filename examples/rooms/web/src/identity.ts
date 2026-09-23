/** Stable per-browser identity: the backend maps it to one Aurix user (`external_id`). */

const DEVICE_KEY = "rooms.device";
const NAME_KEY = "rooms.name";

export function deviceId(): string {
  let id = localStorage.getItem(DEVICE_KEY);
  if (!id || !/^[A-Za-z0-9._~-]{8,128}$/.test(id)) {
    id = crypto.randomUUID();
    localStorage.setItem(DEVICE_KEY, id);
  }
  return id;
}

export function savedName(): string {
  return localStorage.getItem(NAME_KEY) ?? "";
}

export function saveName(name: string): void {
  localStorage.setItem(NAME_KEY, name);
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  const first = parts[0]?.[0] ?? "?";
  const second = parts.length > 1 ? parts[parts.length - 1]?.[0] ?? "" : "";
  return (first + second).toUpperCase();
}

/** Deterministic muted hue per user so avatars are telling without being loud. */
export function hueFor(id: string): number {
  let h = 0;
  for (let i = 0; i < id.length; i += 1) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return h % 360;
}
