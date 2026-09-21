import { expect, test, type Browser } from "@playwright/test";

import { LOCALE_STATE, pinEnglish, signIn, trackErrors, uniq } from "./env";

const viewer = {
  email: `${uniq("e2e-viewer")}@example.com`,
  password: `viewer-${uniq("pw")}-0123456789`,
};

async function asViewer(browser: Browser) {
  const ctx = await browser.newContext({ storageState: LOCALE_STATE });
  const page = await ctx.newPage();
  await pinEnglish(page);
  await signIn(page, viewer.email, viewer.password);
  return { ctx, page };
}

test.describe.serial("administrators, roles and audit", () => {
  test("superadmin creates a viewer; the action lands in the audit log", async ({ page }) => {
    const errors = trackErrors(page);
    await page.goto("/settings?tab=admins");
    await expect(page.getByRole("heading", { name: "Administration" })).toBeVisible();

    await page.getByRole("button", { name: "New administrator" }).click();
    await page.locator("#admin-email").fill(viewer.email);
    await page.locator("#admin-name").fill("E2E Viewer");
    await page.locator("#admin-role").selectOption("viewer");
    await page.locator("#admin-password").fill(viewer.password);
    await page.getByTestId("submit").click();
    await expect(page.getByText("Administrator created")).toBeVisible();
    const row = page.getByRole("row", { name: new RegExp(viewer.email) });
    await expect(row).toBeVisible();
    await expect(row.getByText("Viewer", { exact: true })).toBeVisible();

    await page.getByRole("tab", { name: "Audit log" }).click();
    await page.getByLabel("Action").selectOption("admin_created");
    const entry = page.getByRole("row", { name: /admin_created/ }).first();
    await expect(entry).toBeVisible();
    await entry.click();
    const details = page.getByRole("dialog");
    await expect(details).toBeVisible();
    await expect(details.getByText("Actor")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(details).toBeHidden();

    errors.assertClean();
  });

  test("the viewer sees read-only sections and is denied everything else — by the server", async ({ browser }) => {
    const { ctx, page } = await asViewer(browser);
    const errors = trackErrors(page);
    try {
      await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
      const nav = page.getByRole("navigation", { name: "Menu" });
      await expect(nav.getByRole("link", { name: "Applications" })).toBeVisible();
      await expect(nav.getByRole("link", { name: "Nodes" })).toBeVisible();
      await expect(nav.getByRole("link", { name: "Moderation" })).toHaveCount(0);
      await expect(nav.getByRole("link", { name: "Configuration" })).toHaveCount(0);

      await page.goto("/apps");
      await expect(page.getByRole("heading", { name: "Applications" })).toBeVisible();
      await expect(page.getByRole("button", { name: "New application" })).toHaveCount(0);

      // Deep-linking into a hidden section does not bypass the permission: the API says 403.
      await page.goto("/config");
      await expect(page.getByText(/forbidden|config:read/i).first()).toBeVisible();

      const token = (JSON.parse((await page.evaluate(() => localStorage.getItem("aurix.auth"))) ?? "{}") as { token?: string }).token;
      const denied = await page.request.get("/admin/config", { headers: { authorization: `Bearer ${token}` } });
      expect(denied.status()).toBe(403);
      const allowed = await page.request.get("/v1/nodes", { headers: { authorization: `Bearer ${token}` } });
      expect(allowed.ok()).toBeTruthy();

      errors.assertClean();
    } finally {
      await ctx.close();
    }
  });

  test("superadmin revokes the viewer's tokens and deactivates the account", async ({ page, browser }) => {
    const { ctx, page: viewerPage } = await asViewer(browser);
    try {
      await page.goto("/settings?tab=admins");
      const row = page.getByRole("row", { name: new RegExp(viewer.email) });
      await row.getByRole("button", { name: "Actions" }).click();
      await page.getByRole("menuitem", { name: "Revoke tokens" }).click();
      await page.getByTestId("confirm").click();

      // The viewer's existing session is dead on the next request.
      await viewerPage.goto("/nodes");
      await expect(viewerPage).toHaveURL(/\/login/);

      await row.getByRole("button", { name: "Actions" }).click();
      await page.getByRole("menuitem", { name: "Deactivate" }).click();
      await page.getByTestId("confirm").click();
      // Inactive accounts leave the default list and reappear behind the "Show inactive" toggle.
      await expect(row).toHaveCount(0);
      await page.getByLabel(/Show inactive/).check();
      await expect(row.getByText("Inactive", { exact: true })).toBeVisible();

      await viewerPage.goto("/login");
      await viewerPage.getByLabel("Email").fill(viewer.email);
      await viewerPage.getByLabel("Password").fill(viewer.password);
      await viewerPage.getByRole("button", { name: "Sign in" }).click();
      await expect(viewerPage.getByText("Wrong email or password").or(viewerPage.getByText(/inactive|deactivated/i))).toBeVisible();
      await expect(viewerPage).toHaveURL(/\/login/);
    } finally {
      await ctx.close();
    }
  });
});
