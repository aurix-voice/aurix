import type { Page } from "@playwright/test";

function required(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is required for the dashboard e2e suite`);
  return v;
}

export const admin = {
  get email() {
    return required("AURIX_E2E_ADMIN_EMAIL");
  },
  get password() {
    return required("AURIX_E2E_ADMIN_PASSWORD");
  },
  bootstrapToken: process.env.AURIX_E2E_BOOTSTRAP_TOKEN,
};

export const ADMIN_STATE = "e2e/.auth/admin.json";
export const LOCALE_STATE = { origins: [] as never[], cookies: [] as never[] };

/** Short unique suffix so re-runs against the same node never collide on names. */
export function uniq(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
}

/** Pins the UI language so text selectors are stable regardless of the browser locale. */
export async function pinEnglish(page: Page): Promise<void> {
  await page.addInitScript(() => {
    try {
      localStorage.setItem("aurix.locale", "en");
      localStorage.setItem("aurix.theme", "light");
    } catch {
      /* storage disabled */
    }
  });
}

export async function signIn(page: Page, email: string, password: string): Promise<void> {
  await page.goto("/login");
  await page.getByLabel("Email").fill(email);
  await page.getByLabel("Password").fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.waitForURL((u) => !u.pathname.startsWith("/login"));
}

/** Bearer token of the signed-in administrator, as persisted by the dashboard. */
export async function adminToken(page: Page): Promise<string> {
  const token = await page.evaluate(() => {
    const raw = localStorage.getItem("aurix.auth");
    if (!raw) return null;
    const parsed = JSON.parse(raw) as { token?: string };
    return parsed.token ?? null;
  });
  if (!token) throw new Error("no admin session in localStorage — did the setup project run?");
  return token;
}

/** Creates a throwaway application through the API and makes it the dashboard's selected scope. */
export async function useFreshApp(page: Page, name = uniq("e2e-app")): Promise<{ id: string; name: string }> {
  await page.goto("/");
  const token = await adminToken(page);
  const res = await page.request.post("/v1/apps", { headers: { authorization: `Bearer ${token}` }, data: { name } });
  if (!res.ok()) throw new Error(`POST /v1/apps failed: ${res.status()} ${await res.text()}`);
  const created = (await res.json()) as { id: string };
  await page.evaluate((id) => localStorage.setItem("aurix.app", id), created.id);
  return { id: created.id, name };
}

/** Collects uncaught page errors; call `assertClean()` at the end of a spec. */
export function trackErrors(page: Page): { assertClean: () => void } {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(`console.error: ${m.text()}`);
  });
  return {
    assertClean() {
      const real = errors.filter((e) => !/favicon|Download the React DevTools/i.test(e));
      if (real.length) throw new Error(`browser errors:\n${real.join("\n")}`);
    },
  };
}
