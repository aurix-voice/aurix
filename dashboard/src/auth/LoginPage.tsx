import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { KeyRound } from "lucide-react";
import { useState, type FormEvent } from "react";

import { errorKind, errorMessage, makeClient } from "@/api/client";
import { qk } from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { Button } from "@/ui/Button";
import { Input } from "@/ui/Input";
import { Callout, Card, Field, Skeleton } from "@/ui/Primitives";

function oidcLoginUrl(loginUrl: string, returnTo: string | undefined): string {
  // The node reports its own login URL; we go through the dashboard origin so the
  // proxy (and the state cookie) stay on one host.
  const u = new URL(loginUrl, window.location.origin);
  u.protocol = window.location.protocol;
  u.host = window.location.host;
  if (returnTo) u.searchParams.set("return_to", returnTo);
  return u.toString();
}

export function LoginPage() {
  const { t, locale } = useI18n();
  const { signIn, lastSignOutReason } = useAuth();
  const navigate = useNavigate();
  const search = useSearch({ from: "/auth/login" });

  const methods = useQuery({
    queryKey: qk.authMethods,
    queryFn: () => makeClient(null, null).adminAuthMethods(),
    retry: 1,
    staleTime: 60_000,
  });

  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await signIn(email.trim(), password);
      void navigate({ to: search.redirect ?? "/", replace: true });
    } catch (err) {
      setError(errorKind(err) === "unauthorized" ? t("auth.invalid") : errorMessage(err, locale));
    } finally {
      setBusy(false);
    }
  }

  const passwordEnabled = methods.data?.password_login ?? true;
  const oidc = methods.data?.oidc;

  return (
    <Card className="p-6">
      <h1 className="text-lg font-semibold tracking-tight">{t("auth.title")}</h1>
      <p className="text-[13px] text-fg-muted mt-1">{t("auth.subtitle")}</p>

      {lastSignOutReason === "expired" ? (
        <Callout tone="warn" className="mt-4">
          {t("auth.sessionExpired")}
        </Callout>
      ) : null}

      <div className="mt-5 space-y-4">
        {methods.isPending ? (
          <div className="space-y-3">
            <Skeleton className="h-9" />
            <Skeleton className="h-9" />
            <Skeleton className="h-9" />
          </div>
        ) : methods.isError ? (
          <Callout tone="danger" title={t("auth.methodsUnavailable")}>
            {t("auth.methodsUnavailable.desc")}
          </Callout>
        ) : null}

        {methods.data && !passwordEnabled && !oidc ? <Callout tone="warn">{t("auth.noMethods")}</Callout> : null}

        {oidc ? (
          <Button
            variant="secondary"
            className="w-full"
            onClick={() => {
              window.location.assign(oidcLoginUrl(oidc.login_url, search.redirect));
            }}
          >
            <KeyRound className="size-4" />
            {t("auth.sso", { issuer: issuerLabel(oidc.issuer) })}
          </Button>
        ) : null}

        {oidc && passwordEnabled ? (
          <div className="flex items-center gap-3 text-[11px] uppercase tracking-wide text-fg-faint">
            <span className="h-px flex-1 bg-border" />
            {t("auth.or")}
            <span className="h-px flex-1 bg-border" />
          </div>
        ) : null}

        {passwordEnabled ? (
          <form onSubmit={onSubmit} className="space-y-3" noValidate>
            <Field label={t("auth.email")} htmlFor="login-email">
              <Input
                id="login-email"
                type="email"
                name="email"
                autoComplete="username"
                required
                autoFocus
                value={email}
                onChange={(e) => setEmail(e.target.value)}
              />
            </Field>
            <Field label={t("auth.password")} htmlFor="login-password">
              <Input
                id="login-password"
                type="password"
                name="password"
                autoComplete="current-password"
                required
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </Field>
            {error ? <Callout tone="danger">{error}</Callout> : null}
            <Button type="submit" className="w-full" loading={busy} disabled={!email || !password}>
              {busy ? t("auth.signingIn") : t("auth.signIn")}
            </Button>
          </form>
        ) : methods.data ? (
          oidc ? (
            <p className="text-[12px] text-fg-muted">{t("auth.passwordDisabled")}</p>
          ) : null
        ) : null}
      </div>

      <div className="mt-6 pt-4 border-t border-border text-center">
        <Link to="/setup" className="text-[12px] text-fg-muted hover:text-fg underline-offset-4 hover:underline">
          {t("auth.setup.link")}
        </Link>
      </div>
    </Card>
  );
}

function issuerLabel(issuer: string | null | undefined): string {
  if (!issuer) return "OIDC";
  try {
    return new URL(issuer).hostname;
  } catch {
    return issuer;
  }
}
