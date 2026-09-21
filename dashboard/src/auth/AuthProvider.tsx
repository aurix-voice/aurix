import { useQueryClient } from "@tanstack/react-query";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import { AurixError, makeClient, type T } from "@/api/client";

import { authStore, hasAppPermission, hasPermission, useAuthSession, type AppPermission, type AuthSession } from "./store";

export type AuthStatus = "anonymous" | "authenticated";

interface AuthCtx {
  status: AuthStatus;
  session: AuthSession | null;
  admin: T.Admin | null;
  token: string | null;
  signIn: (email: string, password: string) => Promise<T.Admin>;
  acceptToken: (token: string, expiresInSecs: number) => Promise<T.Admin>;
  signOut: (reason?: "expired" | "manual") => void;
  refreshAdmin: () => Promise<void>;
  can: (perm: T.AdminPermission) => boolean;
  /** Tenant-scoped permission the admin's role delegates on `/v1/*` with `X-Aurix-App`. */
  canApp: (perm: AppPermission) => boolean;
  /** Set when the session was ended because the token expired or was rejected. */
  lastSignOutReason: "expired" | "manual" | null;
}

const Ctx = createContext<AuthCtx | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const session = useAuthSession();
  const qc = useQueryClient();
  const [lastSignOutReason, setLastSignOutReason] = useState<"expired" | "manual" | null>(null);

  const signOut = useCallback(
    (reason: "expired" | "manual" = "manual") => {
      setLastSignOutReason(reason);
      authStore.clear();
      qc.clear();
    },
    [qc],
  );

  const loadAdmin = useCallback(async (token: string): Promise<T.Admin> => {
    const me = await makeClient(token, null).adminMe();
    return me;
  }, []);

  const acceptToken = useCallback(
    async (token: string, expiresInSecs: number) => {
      const expiresAt = Date.now() + Math.max(1, expiresInSecs) * 1000;
      authStore.set({ token, expiresAt, admin: null });
      setLastSignOutReason(null);
      try {
        const admin = await loadAdmin(token);
        authStore.setAdmin(admin);
        return admin;
      } catch (err) {
        authStore.clear();
        throw err;
      }
    },
    [loadAdmin],
  );

  const signIn = useCallback(
    async (email: string, password: string) => {
      const res = await makeClient(null, null).adminLogin({ email, password });
      const expiresAt = Date.now() + res.expires_in_secs * 1000;
      authStore.set({ token: res.token, expiresAt, admin: res.admin });
      setLastSignOutReason(null);
      return res.admin;
    },
    [],
  );

  const refreshAdmin = useCallback(async () => {
    const s = authStore.get();
    if (!s) return;
    try {
      authStore.setAdmin(await loadAdmin(s.token));
    } catch (err) {
      if (err instanceof AurixError && err.status === 401) signOut("expired");
    }
  }, [loadAdmin, signOut]);

  // Validate a restored token once, then sign out exactly when it expires.
  const validatedToken = useRef<string | null>(null);
  useEffect(() => {
    if (!session) return;
    if (validatedToken.current !== session.token) {
      validatedToken.current = session.token;
      void refreshAdmin();
    }
    const ms = session.expiresAt - Date.now();
    const timer = setTimeout(() => signOut("expired"), Math.max(0, Math.min(ms, 2 ** 31 - 1)));
    return () => clearTimeout(timer);
  }, [session, refreshAdmin, signOut]);

  const can = useCallback((perm: T.AdminPermission) => hasPermission(session?.admin, perm), [session]);
  const canApp = useCallback((perm: AppPermission) => hasAppPermission(session?.admin, perm), [session]);

  const value = useMemo<AuthCtx>(
    () => ({
      status: session ? "authenticated" : "anonymous",
      session,
      admin: session?.admin ?? null,
      token: session?.token ?? null,
      signIn,
      acceptToken,
      signOut,
      refreshAdmin,
      can,
      canApp,
      lastSignOutReason,
    }),
    [session, signIn, acceptToken, signOut, refreshAdmin, can, canApp, lastSignOutReason],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAuth(): AuthCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useAuth outside AuthProvider");
  return ctx;
}
