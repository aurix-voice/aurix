import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

const STORAGE_KEY = "aurix.app";

interface Scope {
  appId: string | null;
  setAppId: (id: string | null) => void;
}

const Ctx = createContext<Scope | null>(null);

function read(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

/** The application whose tenant data the dashboard shows (sent as `X-Aurix-App`). */
export function AppScopeProvider({ children }: { children: ReactNode }) {
  const [appId, setState] = useState<string | null>(read);
  useEffect(() => {
    const on = (e: StorageEvent) => {
      if (e.key === STORAGE_KEY) setState(read());
    };
    window.addEventListener("storage", on);
    return () => window.removeEventListener("storage", on);
  }, []);
  const setAppId = useCallback((id: string | null) => {
    setState(id);
    try {
      if (id) localStorage.setItem(STORAGE_KEY, id);
      else localStorage.removeItem(STORAGE_KEY);
    } catch {
      /* ignore */
    }
  }, []);
  const value = useMemo(() => ({ appId, setAppId }), [appId, setAppId]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useAppScope(): Scope {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useAppScope outside AppScopeProvider");
  return ctx;
}
