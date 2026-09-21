import { expect, test } from "@playwright/test";

import { trackErrors, uniq, useFreshApp } from "./env";

test.describe("live channels", () => {
  test("create a persistent channel, edit its config, find it, delete it", async ({ page }) => {
    const errors = trackErrors(page);
    const name = uniq("e2e-lobby");
    await useFreshApp(page);

    await page.goto("/live");
    await expect(page.getByRole("heading", { name: "Live", exact: true })).toBeVisible();
    await expect(page.getByText(/Live|Connecting|Offline/).first()).toBeVisible();

    await page.getByTestId("new-channel").click();
    await page.getByTestId("channel-name").fill(name);
    await page.getByTestId("submit").click();

    await expect(page).toHaveURL(/\/live\/[0-9a-f-]{36}/);
    await expect(page.getByRole("heading", { name })).toBeVisible();
    await expect(page.getByText("0 /", { exact: false }).first()).toBeVisible();

    // Channel config dialog round-trips through PATCH /v1/channels/{id}.
    await page.getByTestId("edit-config").click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await dialog.locator("#channel-max-participants").fill("12");
    await page.getByTestId("submit").click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText("0 / 12")).toBeVisible();

    // The channel is listed under "All" and reachable through search.
    await page.goto("/live");
    await page.getByRole("tab", { name: "All", exact: true }).click();
    await page.getByTestId("channel-search").fill(name);
    const row = page.getByRole("row", { name: new RegExp(name) });
    await expect(row).toHaveCount(1);
    await row.click();
    await expect(page.getByRole("heading", { name })).toBeVisible();

    await page.getByTestId("row-menu").click();
    await page.getByRole("menuitem", { name: "Delete channel" }).click();
    await page.getByTestId("confirm").click();
    await expect(page).toHaveURL(/\/live(\?.*)?$/);
    await page.getByRole("tab", { name: "All", exact: true }).click();
    await page.getByTestId("channel-search").fill(name);
    await expect(page.getByRole("row", { name: new RegExp(name) })).toHaveCount(0);

    errors.assertClean();
  });
});
