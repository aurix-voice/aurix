import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import { en, type MessageKey, type Messages, type Vars } from "./en";
import { ru } from "./ru";

export type Locale = "en" | "ru";
export type { MessageKey };

const STORAGE_KEY = "aurix.locale";
const dictionaries: Record<Locale, Messages> = { en, ru };

interface I18n {
  locale: Locale;
  setLocale: (l: Locale) => void;
  t: (key: MessageKey, vars?: Vars) => string;
}

const I18nContext = createContext<I18n | null>(null);

function detectLocale(): Locale {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === "en" || stored === "ru") return stored;
  } catch {
    /* private mode */
  }
  return navigator.language.toLowerCase().startsWith("ru") ? "ru" : "en";
}

export function translate(locale: Locale, key: MessageKey, vars?: Vars): string {
  const msg = dictionaries[locale][key] ?? en[key];
  if (typeof msg === "function") return msg(vars ?? {});
  if (!vars) return msg;
  return msg.replace(/\{(\w+)\}/g, (_, k: string) => String(vars[k] ?? `{${k}}`));
}

export function I18nProvider({ children, initial }: { children: ReactNode; initial?: Locale }) {
  const [locale, setLocaleState] = useState<Locale>(() => initial ?? detectLocale());
  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);
  const setLocale = useCallback((l: Locale) => {
    setLocaleState(l);
    try {
      localStorage.setItem(STORAGE_KEY, l);
    } catch {
      /* ignore */
    }
  }, []);
  const t = useCallback((key: MessageKey, vars?: Vars) => translate(locale, key, vars), [locale]);
  const value = useMemo(() => ({ locale, setLocale, t }), [locale, setLocale, t]);
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18n {
  const ctx = useContext(I18nContext);
  if (!ctx) throw new Error("useI18n outside I18nProvider");
  return ctx;
}

export function useT(): I18n["t"] {
  return useI18n().t;
}

/** Russian plural category → one/few/many; English → one/other. */
export function plural(locale: Locale, n: number, forms: { one: string; few?: string; many: string }): string {
  const cat = new Intl.PluralRules(locale === "ru" ? "ru-RU" : "en-US").select(n);
  if (cat === "one") return forms.one;
  if (cat === "few" && forms.few) return forms.few;
  return forms.many;
}
