import { expect, test } from "@playwright/test";

// The dashboard origin must serve the SPA for every UI route and hand the API prefixes to
// the node (Caddy in the image, Vite in development) — including the SSE event stream.
test.describe("same-origin API proxy", () => {
  test("health, readiness and the OpenAPI contract come from the node", async ({ request }) => {
    const health = await request.get("/health");
    expect(health.ok()).toBeTruthy();
    const ready = await request.get("/ready");
    expect(ready.ok()).toBeTruthy();
    expect(await ready.json()).toMatchObject({ status: "ready" });
    const openapi = await request.get("/openapi.json");
    expect(openapi.ok()).toBeTruthy();
    expect(await openapi.json()).toMatchObject({ openapi: expect.stringMatching(/^3\./), info: { title: expect.any(String) } });
  });

  test("unauthenticated API calls are refused by the node, not by the proxy", async ({ request }) => {
    const res = await request.get("/v1/apps", { headers: { authorization: "Bearer not-a-token" } });
    expect(res.status()).toBe(401);
    expect(await res.json()).toMatchObject({ error: { code: "TOKEN_INVALID", message: expect.any(String) } });
  });

  test("deep links and unknown routes render the SPA shell, missing assets stay 404", async ({ request }) => {
    for (const path of ["/settings?tab=audit", "/apps/does-not-exist", "/no/such/route"]) {
      const res = await request.get(path);
      expect(res.status(), path).toBe(200);
      expect(res.headers()["content-type"], path).toMatch(/text\/html/);
      expect(await res.text(), path).toContain('<div id="root">');
    }
    const asset = await request.get("/assets/definitely-missing.js");
    if (process.env.DASHBOARD_URL) {
      // Caddy image: a missing hashed asset must not fall back to the SPA shell.
      expect(asset.status()).toBe(404);
    } else {
      // Vite dev server has SPA fallback for everything; only the image is strict here.
      expect([200, 404]).toContain(asset.status());
    }
  });

  test("the SSE stream is proxied unbuffered", async ({ page, request }) => {
    await page.goto("/");
    const stored = await page.evaluate(() => localStorage.getItem("aurix.auth"));
    const token = (JSON.parse(stored ?? "{}") as { token?: string }).token;
    expect(token).toBeTruthy();
    const auth = { authorization: `Bearer ${token}` };
    const apps = await request.get("/v1/apps", { headers: auth });
    expect(apps.ok()).toBeTruthy();
    const list = (await apps.json()) as { data: Array<{ id: string }> };
    let appId = list.data[0]?.id;
    if (!appId) {
      const created = await request.post("/v1/apps", { headers: auth, data: { name: "dashboard-e2e-sse" } });
      expect(created.ok()).toBeTruthy();
      appId = ((await created.json()) as { id: string }).id;
    }

    // `stream.open` is emitted immediately; a buffering proxy would hold it back until the
    // request timed out.
    const first = await page.evaluate(
      async ([tok, appId]) => {
        const ctl = new AbortController();
        const timer = setTimeout(() => ctl.abort(), 8000);
        try {
          const res = await fetch("/v1/events", {
            headers: { authorization: `Bearer ${tok}`, "x-aurix-app": appId, accept: "text/event-stream" },
            signal: ctl.signal,
          });
          const reader = res.body!.getReader();
          const { value } = await reader.read();
          await reader.cancel();
          return { status: res.status, type: res.headers.get("content-type"), chunk: new TextDecoder().decode(value) };
        } finally {
          clearTimeout(timer);
        }
      },
      [token!, appId] as const,
    );
    expect(first.status).toBe(200);
    expect(first.type).toMatch(/text\/event-stream/);
    expect(first.chunk).toContain("event: stream.open");
  });
});
