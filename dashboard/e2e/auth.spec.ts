import { expect, test } from "@playwright/test";

import { admin, LOCALE_STATE, pinEnglish, signIn } from "./env";

test.use({ storageState: LOCALE_STATE });

test.describe("authentication", () => {
  test.beforeEach(async ({ page }) => pinEnglish(page));

  test("unauthenticated deep links redirect to /login and come back after sign-in", async ({ page }) => {
    await page.goto("/settings?tab=audit");
    await expect(page).toHaveURL(/\/login\?/);
    await expect(page.getByRole("heading", { name: "Sign in" })).toBeVisible();

    await page.getByLabel("Email").fill(admin.email);
    await page.getByLabel("Password").fill(admin.password);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page).toHaveURL(/\/settings\?tab=audit/);
    await expect(page.getByRole("tab", { name: "Audit log" })).toHaveAttribute("aria-selected", "true");
  });

  test("wrong password is rejected without leaking details", async ({ page }) => {
    await page.goto("/login");
    await page.getByLabel("Email").fill(admin.email);
    await page.getByLabel("Password").fill("definitely-not-the-password");
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByText("Wrong email or password")).toBeVisible();
    await expect(page).toHaveURL(/\/login/);
  });

  test("sign out clears the session and protects the app again", async ({ page }) => {
    await signIn(page, admin.email, admin.password);
    await page.getByRole("button", { name: "Profile" }).click();
    await page.getByRole("menuitem", { name: "Sign out" }).click();
    await expect(page).toHaveURL(/\/login/);
    expect(await page.evaluate(() => localStorage.getItem("aurix.auth"))).toBeNull();
    await page.goto("/apps");
    await expect(page).toHaveURL(/\/login/);
  });

  test("/setup is closed once an administrator exists", async ({ page }) => {
    await page.goto("/setup");
    await page.getByLabel("Display name").fill("Intruder");
    await page.getByLabel("Email").fill(`intruder-${Date.now()}@example.com`);
    await page.getByLabel("Password", { exact: true }).fill("intruder-password-0123456789");
    await page.getByRole("button", { name: "Create administrator" }).click();
    await expect(page.getByText(/an administrator already exists/i)).toBeVisible();
    await expect(page).toHaveURL(/\/setup/);
  });
});
