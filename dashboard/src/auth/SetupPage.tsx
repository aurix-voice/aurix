import { Link, useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";

import { errorMessage, makeClient } from "@/api/client";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { Button } from "@/ui/Button";
import { Input } from "@/ui/Input";
import { Callout, Card, Field } from "@/ui/Primitives";

export function SetupPage() {
  const { t, locale } = useI18n();
  const { signIn } = useAuth();
  const navigate = useNavigate();
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [bootstrapToken, setBootstrapToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await makeClient(null, null).adminSetup(
        { email: email.trim(), password, display_name: displayName.trim() },
        bootstrapToken ? { auth: { bootstrapToken } } : undefined,
      );
      await signIn(email.trim(), password);
      void navigate({ to: "/", replace: true });
    } catch (err) {
      setError(errorMessage(err, locale));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className="p-6">
      <h1 className="text-lg font-semibold tracking-tight">{t("auth.setup.title")}</h1>
      <p className="text-[13px] text-fg-muted mt-1">{t("auth.setup.desc")}</p>
      <form onSubmit={onSubmit} className="mt-5 space-y-3" noValidate>
        <Field label={t("auth.setup.displayName")}>
          <Input required autoFocus value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
        </Field>
        <Field label={t("auth.email")}>
          <Input type="email" required autoComplete="username" value={email} onChange={(e) => setEmail(e.target.value)} />
        </Field>
        <Field label={t("auth.password")} hint={t("auth.newPassword.hint")}>
          <Input
            type="password"
            required
            autoComplete="new-password"
            minLength={12}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </Field>
        <Field label={t("auth.setup.bootstrapToken")} hint={t("auth.setup.bootstrapToken.hint")}>
          <Input type="password" autoComplete="off" value={bootstrapToken} onChange={(e) => setBootstrapToken(e.target.value)} />
        </Field>
        {error ? <Callout tone="danger">{error}</Callout> : null}
        <Button type="submit" className="w-full" loading={busy} disabled={!email || !password || !displayName}>
          {t("auth.setup.submit")}
        </Button>
      </form>
      <div className="mt-6 pt-4 border-t border-border text-center">
        <Link to="/login" className="text-[12px] text-fg-muted hover:text-fg underline-offset-4 hover:underline">
          {t("auth.title")}
        </Link>
      </div>
    </Card>
  );
}
