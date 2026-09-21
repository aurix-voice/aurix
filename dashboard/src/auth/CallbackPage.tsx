import { Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { errorMessage, makeClient } from "@/api/client";
import { useAuth } from "@/auth/AuthProvider";
import { useI18n } from "@/i18n";
import { Button } from "@/ui/Button";
import { Callout, Card, Spinner } from "@/ui/Primitives";

interface CallbackResult {
  token: string;
  expiresInSecs: number;
  returnTo: string | undefined;
}

function safePath(p: string | null | undefined): string | undefined {
  return p && p.startsWith("/") && !p.startsWith("//") ? p : undefined;
}

/**
 * Two shapes reach this page:
 *  - `#token=…&expires_in_secs=…[&return_to=…]` — the node redirected here itself
 *    (`auth.oidc.frontend_redirect`); the token never touches the query string or server logs.
 *  - `?code=…&state=…` — the IdP's redirect_uri points at the dashboard; we complete the exchange
 *    through the same-origin proxy, so the node's state cookie is sent along.
 */
async function complete(): Promise<CallbackResult> {
  const hash = new URLSearchParams(window.location.hash.replace(/^#/, ""));
  if (hash.has("error")) {
    throw new Error(hash.get("error_description") || hash.get("error") || "error");
  }
  const token = hash.get("token");
  if (token) {
    return {
      token,
      expiresInSecs: Number(hash.get("expires_in_secs") ?? "0") || 3600,
      returnTo: safePath(hash.get("return_to")),
    };
  }
  const query = new URLSearchParams(window.location.search);
  const res = await makeClient(null, null).adminOidcCallback({
    code: query.get("code") ?? undefined,
    state: query.get("state") ?? undefined,
    error: query.get("error") ?? undefined,
    error_description: query.get("error_description") ?? undefined,
  });
  return { token: res.token, expiresInSecs: res.expires_in_secs, returnTo: safePath(res.return_to) };
}

export function CallbackPage() {
  const { t, locale } = useI18n();
  const { acceptToken } = useAuth();
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);
  const started = useRef(false);

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void (async () => {
      try {
        const r = await complete();
        // Scrub the token from history before anything else renders.
        window.history.replaceState(null, "", window.location.pathname);
        await acceptToken(r.token, r.expiresInSecs);
        void navigate({ to: r.returnTo ?? "/", replace: true });
      } catch (err) {
        setError(errorMessage(err, locale));
      }
    })();
  }, [acceptToken, navigate, locale]);

  return (
    <Card className="p-6">
      {error ? (
        <>
          <Callout tone="danger" title={t("auth.callback.error")}>
            {error}
          </Callout>
          <Link to="/login" className="block mt-4">
            <Button variant="secondary" className="w-full">
              {t("auth.title")}
            </Button>
          </Link>
        </>
      ) : (
        <div className="flex items-center gap-3 text-[13px] text-fg-muted">
          <Spinner />
          {t("auth.callback.title")}
        </div>
      )}
    </Card>
  );
}
