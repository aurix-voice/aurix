/**
 * Region discovery: ask the platform which regions have capacity, measure the round-trip time
 * to each one from the player's network and pick the endpoint to connect to. The server orders
 * regions by preference, distance and load; the client re-ranks by measured RTT because
 * geography is only a proxy for latency.
 */

export interface GeoLocation {
  latitude: number;
  longitude: number;
}

/** One advertised region (`GET /v1/me/regions`, `endpoint` in `POST /v1/tokens`). */
export interface RegionEndpoint {
  region: string;
  node_id: string;
  /** Direct `wss://` URL of the least-loaded node in the region; pass as `wsUrl`. */
  ws_url: string;
  /** `GET` this to measure RTT (`/health` of the same node); `null` when the node has no public API URL. */
  probe_url: string | null;
  location: GeoLocation | null;
  /** Great-circle distance from the `location` hint, when one was given. */
  distance_km: number | null;
  /** Nodes with capacity in the region. */
  nodes: number;
  /** Load of the advertised node, `0..1`. */
  load_factor: number;
}

export interface RegionsResponse {
  regions: RegionEndpoint[];
  recommended: RegionEndpoint | null;
}

export interface ProbedRegion extends RegionEndpoint {
  /** Best of the probe samples in milliseconds; `null` when not probed or unreachable. */
  rttMs: number | null;
}

export interface DiscoverRegionsOptions {
  /** REST base URL of any node or the shared API hostname, e.g. `https://voice.example.com`. */
  apiUrl: string;
  /** Player credential (session JWT or `login` action token). */
  token: string;
  /** Region the game prefers (party leader's region, matchmaking result); it ranks first when reachable. */
  region?: string;
  /** Approximate player location for the server's distance ordering. */
  location?: GeoLocation;
  /** Measure RTT to every region's `probe_url`; defaults to `true`. */
  probe?: boolean;
  /** Timed samples per region after one warm-up request; defaults to `3`. */
  probeSamples?: number;
  /** Per-request timeout; defaults to `2000` ms. Regions that time out rank last. */
  probeTimeoutMs?: number;
  /** RTT differences below this are ties broken by the server order (distance, load); defaults to `15` ms. */
  rttToleranceMs?: number;
  /** Injected for tests. */
  fetch?: typeof fetch;
}

export interface DiscoveredRegions {
  /** Ranked best first. */
  regions: ProbedRegion[];
  /** `regions[0]`, or `null` when nothing is advertised. */
  recommended: ProbedRegion | null;
}

export const DEFAULT_PROBE_SAMPLES = 3;
export const DEFAULT_PROBE_TIMEOUT_MS = 2000;
export const DEFAULT_RTT_TOLERANCE_MS = 15;

/** Fetch, probe and rank; see `rankRegions` for the ordering. */
export async function discoverRegions(opts: DiscoverRegionsOptions): Promise<DiscoveredRegions> {
  const doFetch = opts.fetch ?? fetch;
  const params = new URLSearchParams();
  if (opts.region) params.set('region', opts.region);
  if (opts.location) {
    params.set('latitude', String(opts.location.latitude));
    params.set('longitude', String(opts.location.longitude));
  }
  const qs = params.toString();
  const res = await doFetch(`${opts.apiUrl.replace(/\/$/, '')}/v1/me/regions${qs ? `?${qs}` : ''}`, {
    headers: { Authorization: `Bearer ${opts.token}` },
  });
  if (!res.ok) {
    throw new Error(`region discovery failed: HTTP ${res.status}`);
  }
  const body: unknown = await res.json();
  const parsed = parseRegionsResponse(body);

  const probe = opts.probe ?? true;
  const rtts = new Map<string, number | null>();
  if (probe) {
    const samples = opts.probeSamples ?? DEFAULT_PROBE_SAMPLES;
    const timeoutMs = opts.probeTimeoutMs ?? DEFAULT_PROBE_TIMEOUT_MS;
    await Promise.all(
      parsed.regions.map(async (r) => {
        if (!r.probe_url) return;
        rtts.set(r.node_id, await probeRtt(r.probe_url, { samples, timeoutMs, fetch: doFetch }));
      }),
    );
  }
  const rank: RankOptions = {};
  if (opts.region !== undefined) rank.preferred = opts.region;
  if (opts.rttToleranceMs !== undefined) rank.rttToleranceMs = opts.rttToleranceMs;
  const ranked = rankRegions(parsed.regions, rtts, rank);
  return { regions: ranked, recommended: ranked[0] ?? null };
}

export interface ProbeOptions {
  samples?: number;
  timeoutMs?: number;
  fetch?: typeof fetch;
}

/**
 * Minimum round-trip time over `samples` GETs of `url` (one extra warm-up request pays for the
 * TLS handshake and is discarded). `null` when every request failed or timed out.
 */
export async function probeRtt(url: string, opts: ProbeOptions = {}): Promise<number | null> {
  const doFetch = opts.fetch ?? fetch;
  const samples = Math.max(1, opts.samples ?? DEFAULT_PROBE_SAMPLES);
  const timeoutMs = opts.timeoutMs ?? DEFAULT_PROBE_TIMEOUT_MS;
  let best: number | null = null;
  for (let i = 0; i <= samples; i++) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    const started = performance.now();
    try {
      const res = await doFetch(url, { method: 'GET', cache: 'no-store', signal: controller.signal });
      // Drain the body so the connection is reusable and the timing covers the whole response.
      await res.arrayBuffer();
      if (!res.ok) return best;
      const elapsed = performance.now() - started;
      if (i > 0 && (best === null || elapsed < best)) best = elapsed;
    } catch {
      // A failed warm-up means the node is unreachable; a failed sample just does not count.
      if (i === 0) return null;
    } finally {
      clearTimeout(timer);
    }
  }
  return best;
}

export interface RankOptions {
  preferred?: string;
  rttToleranceMs?: number;
}

/**
 * Stable re-ranking of the server's list: the preferred region first when it was reachable, then
 * regions with a measured RTT in ascending `rttToleranceMs` buckets, then regions that were not
 * probed, and last regions whose probe failed. Within a bucket the server order (distance, load)
 * is kept.
 */
export function rankRegions(
  regions: RegionEndpoint[],
  rtts: ReadonlyMap<string, number | null>,
  opts: RankOptions = {},
): ProbedRegion[] {
  const tolerance = Math.max(1, opts.rttToleranceMs ?? DEFAULT_RTT_TOLERANCE_MS);
  const probed: ProbedRegion[] = regions.map((r) => {
    const rtt = rtts.get(r.node_id);
    return { ...r, rttMs: typeof rtt === 'number' && Number.isFinite(rtt) ? rtt : null };
  });
  const key = (r: ProbedRegion): number => {
    const measured = r.rttMs !== null;
    const preferred = opts.preferred !== undefined && r.region === opts.preferred;
    // Unreachable preferred regions lose their bonus: a probe that failed is a strong signal.
    if (preferred && (measured || !rtts.has(r.node_id))) return -1;
    if (!measured) return rtts.has(r.node_id) ? Number.MAX_SAFE_INTEGER : Number.MAX_SAFE_INTEGER - 1;
    return Math.floor((r.rttMs as number) / tolerance);
  };
  return probed
    .map((r, index) => ({ r, index, k: key(r) }))
    .sort((a, b) => a.k - b.k || a.index - b.index)
    .map((x) => x.r);
}

export function parseRegionsResponse(body: unknown): RegionsResponse {
  if (typeof body !== 'object' || body === null || !('regions' in body)) {
    throw new Error('region discovery: malformed response');
  }
  const raw = (body as { regions: unknown; recommended?: unknown }).regions;
  if (!Array.isArray(raw)) throw new Error('region discovery: malformed response');
  const regions = raw.map(parseEndpoint);
  const rec = (body as { recommended?: unknown }).recommended;
  return { regions, recommended: rec === null || rec === undefined ? null : parseEndpoint(rec) };
}

function parseEndpoint(v: unknown): RegionEndpoint {
  if (typeof v !== 'object' || v === null) throw new Error('region discovery: malformed endpoint');
  const o = v as Record<string, unknown>;
  if (typeof o.region !== 'string' || typeof o.node_id !== 'string' || typeof o.ws_url !== 'string') {
    throw new Error('region discovery: malformed endpoint');
  }
  const loc = o.location;
  const location =
    typeof loc === 'object' &&
    loc !== null &&
    typeof (loc as GeoLocation).latitude === 'number' &&
    typeof (loc as GeoLocation).longitude === 'number'
      ? { latitude: (loc as GeoLocation).latitude, longitude: (loc as GeoLocation).longitude }
      : null;
  return {
    region: o.region,
    node_id: o.node_id,
    ws_url: o.ws_url,
    probe_url: typeof o.probe_url === 'string' ? o.probe_url : null,
    location,
    distance_km: typeof o.distance_km === 'number' ? o.distance_km : null,
    nodes: typeof o.nodes === 'number' ? o.nodes : 0,
    load_factor: typeof o.load_factor === 'number' ? o.load_factor : 0,
  };
}
