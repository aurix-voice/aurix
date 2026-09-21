import { defineConfig, devices } from "@playwright/test";

// Live end-to-end suite: drives the real dashboard (Caddy image or Vite dev server) against a
// real Aurix node. Nothing is mocked; every spec goes through the same-origin API proxy.
//
//   DASHBOARD_URL                dashboard origin under test (default: Vite dev server started here)
//   AURIX_API_URL                node the dev server proxies to (default http://127.0.0.1:8080)
//   AURIX_E2E_ADMIN_EMAIL        administrator credentials (created via /setup when
//   AURIX_E2E_ADMIN_PASSWORD     AURIX_E2E_BOOTSTRAP_TOKEN is set and no admin exists yet)
//   AURIX_E2E_BOOTSTRAP_TOKEN    `auth.admin_bootstrap_token` of the node, optional
const dashboardUrl = process.env.DASHBOARD_URL;
const baseURL = dashboardUrl ?? "http://127.0.0.1:5173";

export default defineConfig({
  testDir: "./e2e",
  outputDir: "./test-results",
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  timeout: 45_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : [["list"]],
  use: {
    baseURL,
    locale: "en-US",
    timezoneId: "UTC",
    viewport: { width: 1440, height: 900 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "setup", testMatch: /auth\.setup\.ts/ },
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"], storageState: "e2e/.auth/admin.json" },
      dependencies: ["setup"],
    },
  ],
  webServer: dashboardUrl
    ? undefined
    : {
        command: "npm run dev -- --host 127.0.0.1 --port 5173 --strictPort",
        url: "http://127.0.0.1:5173/login",
        reuseExistingServer: !process.env.CI,
        timeout: 60_000,
      },
});
