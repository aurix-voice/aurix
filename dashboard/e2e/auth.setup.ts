import { expect, test as setup } from "@playwright/test";

import { admin, ADMIN_STATE, pinEnglish, signIn } from "./env";

// Signs the administrator in once and shares the browser state with every other spec.
// On a fresh node (CI) the first administrator is created through the /setup page itself,
// so the bootstrap flow of the UI is part of what the suite verifies.
setup("administrator is signed in", async ({ page, request }) => {
  await pinEnglish(page);

  if (admin.bootstrapToken) {
    const probe = await request.post("/admin/login", { data: { email: admin.email, password: admin.password } });
    if (probe.status() === 401) {
      await page.goto("/setup");
      await expect(page.getByRole("heading", { name: "Create the first administrator" })).toBeVisible();
      await page.getByLabel("Display name").fill("Dashboard E2E");
      await page.getByLabel("Email").fill(admin.email);
      await page.getByLabel("Password", { exact: true }).fill(admin.password);
      await page.getByLabel("Bootstrap token").fill(admin.bootstrapToken);
      await page.getByRole("button", { name: "Create administrator" }).click();
      await page.waitForURL((u) => u.pathname === "/");
      await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
      await page.context().storageState({ path: ADMIN_STATE });
      return;
    }
  }

  await signIn(page, admin.email, admin.password);
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
  await page.context().storageState({ path: ADMIN_STATE });
});
