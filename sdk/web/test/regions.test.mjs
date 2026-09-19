import { test } from 'node:test';
import assert from 'node:assert/strict';
import { discoverRegions, parseRegionsResponse, probeRtt, rankRegions } from '../dist/regions.js';

const ep = (region, node_id, extra = {}) => ({
  region,
  node_id,
  ws_url: `wss://${node_id}.example/ws`,
  probe_url: `https://${node_id}.example/health`,
  location: null,
  distance_km: null,
  nodes: 1,
  load_factor: 0.1,
  ...extra,
});

test('rankRegions: preferred first, then RTT buckets, unmeasured last, stable within ties', () => {
  const regions = [ep('eu_west', 'eu1'), ep('eu_central', 'eu2'), ep('us_east', 'us1'), ep('africa', 'af1', { probe_url: null })];
  const rtts = new Map([
    ['eu1', 40],
    ['eu2', 30],
    ['us1', 120],
  ]);
  // 30 and 40 share the 15 ms bucket [30, 45): server order (eu1 first) wins.
  assert.deepEqual(rankRegions(regions, rtts).map((r) => r.node_id), ['eu1', 'eu2', 'us1', 'af1']);
  assert.deepEqual(rankRegions(regions, rtts, { rttToleranceMs: 5 }).map((r) => r.node_id), ['eu2', 'eu1', 'us1', 'af1']);
  assert.deepEqual(rankRegions(regions, rtts, { preferred: 'us_east' }).map((r) => r.node_id), ['us1', 'eu1', 'eu2', 'af1']);
  // A preferred region that failed its probe loses the bonus.
  const dead = new Map(rtts);
  dead.set('us1', null);
  assert.deepEqual(rankRegions(regions, dead, { preferred: 'us_east' }).map((r) => r.node_id), ['eu1', 'eu2', 'af1', 'us1']);
  // Preferred but never probed (no probe_url) still ranks first.
  assert.deepEqual(rankRegions(regions, rtts, { preferred: 'africa' }).map((r) => r.node_id), ['af1', 'eu1', 'eu2', 'us1']);
  assert.equal(rankRegions(regions, rtts)[0].rttMs, 40);
});

test('parseRegionsResponse validates the wire shape', () => {
  const parsed = parseRegionsResponse({
    regions: [ep('eu_west', 'eu1', { location: { latitude: 48.8, longitude: 2.3 }, distance_km: 12.5 })],
    recommended: ep('eu_west', 'eu1'),
  });
  assert.equal(parsed.regions[0].location.latitude, 48.8);
  assert.equal(parsed.regions[0].distance_km, 12.5);
  assert.equal(parsed.recommended.node_id, 'eu1');
  assert.equal(parseRegionsResponse({ regions: [], recommended: null }).recommended, null);
  assert.throws(() => parseRegionsResponse({}));
  assert.throws(() => parseRegionsResponse({ regions: [{ region: 'x' }] }));
});

const fakeFetch = (handler) => async (url, init) => handler(String(url), init ?? {});
const okJson = (body) => ({ ok: true, status: 200, json: async () => body, arrayBuffer: async () => new ArrayBuffer(0) });

test('probeRtt discards the warm-up, keeps the minimum and reports unreachable nodes', async () => {
  let calls = 0;
  const rtt = await probeRtt('https://n/health', { samples: 3, fetch: fakeFetch(async () => { calls++; return okJson({}); }) });
  assert.equal(calls, 4);
  assert.ok(rtt !== null && rtt >= 0);
  const dead = await probeRtt('https://n/health', { samples: 2, fetch: fakeFetch(async () => { throw new Error('ECONNREFUSED'); }) });
  assert.equal(dead, null);
  const notOk = await probeRtt('https://n/health', { samples: 2, fetch: fakeFetch(async () => ({ ok: false, status: 503, arrayBuffer: async () => new ArrayBuffer(0) })) });
  assert.equal(notOk, null);
});

test('discoverRegions sends the hint, bearer token and probes every region', async () => {
  const seen = [];
  const f = fakeFetch(async (url, init) => {
    seen.push(url);
    if (url.includes('/v1/me/regions')) {
      assert.equal(init.headers.Authorization, 'Bearer tok');
      return okJson({ regions: [ep('eu_west', 'eu1'), ep('us_east', 'us1')], recommended: ep('eu_west', 'eu1') });
    }
    if (url.includes('us1')) await new Promise((r) => setTimeout(r, 40));
    return okJson({ status: 'healthy' });
  });
  const out = await discoverRegions({
    apiUrl: 'https://api.example/',
    token: 'tok',
    region: 'us_east',
    location: { latitude: 1.5, longitude: -2 },
    probeSamples: 1,
    fetch: f,
  });
  assert.equal(seen[0], 'https://api.example/v1/me/regions?region=us_east&latitude=1.5&longitude=-2');
  assert.equal(seen.filter((u) => u.endsWith('/health')).length, 4);
  // Preferred region ranks first even though it is slower...
  assert.equal(out.recommended.node_id, 'us1');
  assert.ok(out.regions[0].rttMs >= 35);
  // ...and without a preference the faster one wins.
  const fast = await discoverRegions({ apiUrl: 'https://api.example', token: 'tok', probeSamples: 1, fetch: f });
  assert.equal(fast.recommended.node_id, 'eu1');
  const off = await discoverRegions({ apiUrl: 'https://api.example', token: 'tok', probe: false, fetch: f });
  assert.equal(off.regions[0].rttMs, null);
  await assert.rejects(
    discoverRegions({ apiUrl: 'https://api.example', token: 'tok', fetch: fakeFetch(async () => ({ ok: false, status: 401 })) }),
    /HTTP 401/,
  );
});
