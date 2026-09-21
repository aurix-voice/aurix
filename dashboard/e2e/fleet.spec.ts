import { expect, test } from "@playwright/test";

import { trackErrors, useFreshApp } from "./env";

test.describe("overview, nodes and configuration", () => {
  test("overview shows fleet health and every section renders without browser errors", async ({ page }) => {
    const errors = trackErrors(page);
    await useFreshApp(page);
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
    await expect(page.getByText("Concurrent sessions").first()).toBeVisible();
    await expect(page.getByText(/\d+ of \d+ healthy/)).toBeVisible();

    for (const [path, heading] of [
      ["/nodes", "Nodes"],
      ["/apps", "Applications"],
      ["/live", "Live"],
      ["/moderation", "Moderation"],
      ["/recordings", "Recordings"],
      ["/analytics", "Analytics"],
      ["/settings", "Administration"],
      ["/config", "Effective configuration"],
    ] as const) {
      await page.goto(path);
      await expect(page.getByRole("heading", { name: heading, exact: true }).first(), path).toBeVisible();
      await expect(page.getByText(/Something went wrong|Cannot reach the API/), path).toHaveCount(0);
    }
    errors.assertClean();
  });

  test("drain and undrain a node from the fleet table, audited", async ({ page }) => {
    const errors = trackErrors(page);
    await page.goto("/nodes");
    const table = page.getByRole("table");
    await expect(table.getByRole("row")).not.toHaveCount(0);
    const healthy = table.getByRole("row").filter({ has: page.getByRole("cell", { name: "Healthy", exact: true }) }).first();
    await expect(healthy).toBeVisible();
    // The table shows ids shortened; the row carries the full id as its key.
    const nodeId = (await healthy.getAttribute("data-row-key")) ?? "";
    expect(nodeId).toMatch(/^[0-9a-f-]{36}$/);
    const row = table.locator(`tr[data-row-key="${nodeId}"]`);

    if (await row.getByRole("button", { name: "Undrain" }).isVisible()) {
      await row.getByRole("button", { name: "Undrain" }).click();
      await page.getByTestId("confirm").click();
      await expect(row.getByRole("button", { name: "Drain" })).toBeVisible();
    }

    await row.getByRole("button", { name: "Drain" }).click();
    await page.locator("#drain-reason").fill("dashboard e2e");
    await page.getByTestId("submit").click();
    await expect(page.getByText("Drain started")).toBeVisible();
    await expect(row.getByText("Draining")).toBeVisible();
    await expect(row.getByRole("button", { name: "Undrain" })).toBeVisible();

    await row.getByRole("button", { name: "Undrain" }).click();
    await page.getByTestId("confirm").click();
    await expect(page.getByText("Drain cleared")).toBeVisible();
    await expect(row.getByRole("button", { name: "Drain" })).toBeVisible();

    await page.goto("/settings?tab=audit");
    await page.getByLabel("Action").selectOption("node_drained");
    await expect(page.getByRole("row", { name: /node_drained/ }).first()).toBeVisible();
    errors.assertClean();
  });

  test("effective configuration is read-only with secrets masked", async ({ page }) => {
    const errors = trackErrors(page);
    await page.goto("/config");
    await expect(page.getByRole("heading", { name: "Effective configuration" })).toBeVisible();
    await expect(page.getByText("Secrets are masked", { exact: false })).toBeVisible();
    await expect(page.getByText(/\d+ masked/).first()).toBeVisible();

    const search = page.getByLabel("Filter keys…");
    await search.fill("redis");
    await expect(page.getByText(/redis/i).first()).toBeVisible();
    await expect(page.getByRole("main").locator("textarea, [contenteditable=true]")).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^(Save|Apply|Edit)$/ })).toHaveCount(0);

    // The masked value never reaches the browser.
    const html = await page.content();
    expect(html).not.toMatch(/postgres(ql)?:\/\/[^*\s"]+:[^*\s"]+@/);
    errors.assertClean();
  });
});
