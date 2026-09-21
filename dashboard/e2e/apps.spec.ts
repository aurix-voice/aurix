import { expect, test } from "@playwright/test";

import { trackErrors, uniq } from "./env";

test.describe("applications and keys", () => {
  test("create an application, read its key once, add and revoke an API key", async ({ page }) => {
    const errors = trackErrors(page);
    const name = uniq("e2e-app");

    await page.goto("/apps");
    await expect(page.getByRole("heading", { name: "Applications" })).toBeVisible();
    await page.getByRole("button", { name: "New application" }).first().click();
    await page.locator("#app-name").fill(name);
    await page.locator("#app-desc").fill("created by the dashboard e2e suite");
    await page.locator("#app-mc").fill("50");
    await page.getByTestId("submit").click();

    // The first API key is shown exactly once.
    const reveal = page.getByRole("dialog", { name: /Application created/ });
    await expect(reveal).toBeVisible();
    await expect(reveal.getByText("This is the only time the API key is shown", { exact: false })).toBeVisible();
    const apiKey = (await reveal.locator("pre, code").first().innerText()).trim();
    expect(apiKey.length).toBeGreaterThan(20);
    await reveal.getByRole("button", { name: "Close" }).last().click();

    await expect(page).toHaveURL(/\/apps\/[0-9a-f-]{36}/);
    await expect(page.getByRole("heading", { name })).toBeVisible();
    await expect(page.getByText(apiKey)).toHaveCount(0);

    // The key authenticates against the node through the same proxy.
    const withKey = await page.request.get("/v1/channels", { headers: { "x-api-key": apiKey } });
    expect(withKey.ok()).toBeTruthy();

    await page.getByRole("tab", { name: "API keys" }).click();
    await expect(page.getByRole("table")).toBeVisible();
    await page.getByRole("button", { name: "New key" }).click();
    const keyName = uniq("token-server");
    await page.locator("#key-name").fill(keyName);
    await page.getByTestId("submit").click();
    const created = page.getByRole("dialog", { name: /Key created/ });
    await expect(created).toBeVisible();
    await created.getByRole("button", { name: "Close" }).last().click();
    const row = page.getByRole("row", { name: new RegExp(keyName) });
    await expect(row).toBeVisible();

    await row.getByRole("button", { name: "Revoke" }).click();
    await page.getByTestId("confirm").click();
    await expect(row.getByText("Revoked")).toBeVisible();

    // Settings tab: quotas round-trip through PATCH.
    await page.getByRole("tab", { name: "Settings" }).click();
    const mc = page.locator("#app-mc");
    await expect(mc).toHaveValue("50");
    await mc.fill("75");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("Application saved")).toBeVisible();
    await page.reload();
    await page.getByRole("tab", { name: "Settings" }).click();
    await expect(page.locator("#app-mc")).toHaveValue("75");

    // Search finds the new tenant in the list.
    await page.goto("/apps");
    await page.getByRole("searchbox").or(page.getByLabel("Search")).first().fill(name);
    await expect(page.getByRole("row", { name: new RegExp(name) })).toHaveCount(1);

    errors.assertClean();
  });
});
