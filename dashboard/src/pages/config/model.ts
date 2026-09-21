export const MASK = "***";

export type Scalar = string | number | boolean | null;

export interface ConfigLeaf {
  /** Dotted path below the section, e.g. `redis.sentinel.master`. */
  path: string;
  value: Scalar | Scalar[];
  masked: boolean;
}

export interface ConfigSection {
  name: string;
  leaves: ConfigLeaf[];
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isScalar(v: unknown): v is Scalar {
  return v === null || typeof v === "string" || typeof v === "number" || typeof v === "boolean";
}

function flatten(prefix: string, value: unknown, out: ConfigLeaf[]): void {
  if (isRecord(value)) {
    const keys = Object.keys(value).sort();
    if (keys.length === 0) out.push({ path: prefix, value: [], masked: false });
    for (const k of keys) flatten(prefix ? `${prefix}.${k}` : k, value[k], out);
    return;
  }
  if (Array.isArray(value)) {
    if (value.every(isScalar)) {
      out.push({ path: prefix, value, masked: value.some((v) => v === MASK) });
      return;
    }
    value.forEach((v, i) => flatten(`${prefix}[${i}]`, v, out));
    return;
  }
  const scalar: Scalar = isScalar(value) ? value : JSON.stringify(value);
  out.push({ path: prefix, value: scalar, masked: scalar === MASK });
}

/** Top-level keys become sections; scalars at the root are grouped under `general`. */
export function configSections(config: Record<string, unknown>): ConfigSection[] {
  const sections: ConfigSection[] = [];
  const general: ConfigLeaf[] = [];
  for (const key of Object.keys(config).sort()) {
    const v = config[key];
    if (isRecord(v)) {
      const leaves: ConfigLeaf[] = [];
      flatten("", v, leaves);
      sections.push({ name: key, leaves });
    } else {
      flatten(key, v, general);
    }
  }
  return general.length ? [{ name: "general", leaves: general }, ...sections] : sections;
}

export function formatLeaf(v: Scalar | Scalar[]): string {
  if (Array.isArray(v)) return v.length ? v.map((x) => formatLeaf(x)).join(", ") : "[]";
  if (v === null) return "null";
  if (typeof v === "string") return v === "" ? '""' : v;
  return String(v);
}

export function filterSections(sections: readonly ConfigSection[], query: string): ConfigSection[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...sections];
  const out: ConfigSection[] = [];
  for (const s of sections) {
    if (s.name.toLowerCase().includes(q)) {
      out.push(s);
      continue;
    }
    const leaves = s.leaves.filter((l) => l.path.toLowerCase().includes(q) || formatLeaf(l.value).toLowerCase().includes(q));
    if (leaves.length) out.push({ name: s.name, leaves });
  }
  return out;
}

export function countMasked(sections: readonly ConfigSection[]): number {
  return sections.reduce((n, s) => n + s.leaves.filter((l) => l.masked).length, 0);
}

export function countLeaves(sections: readonly ConfigSection[]): number {
  return sections.reduce((n, s) => n + s.leaves.length, 0);
}

export function configFilename(nodeId: string, version: string): string {
  const safe = (s: string) => s.replace(/[^a-zA-Z0-9._-]+/g, "_");
  return `aurix-config-${safe(nodeId)}-${safe(version)}.json`;
}
