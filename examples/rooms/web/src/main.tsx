import { StrictMode, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";

import { api, type AppConfig } from "./api";
import { Center } from "./components";
import { I18nContext, STRINGS, detectLang, saveLang, type Lang } from "./i18n";
import { Lobby } from "./pages/Lobby";
import { Room } from "./pages/Room";
import { useRoute } from "./router";
import "./styles.css";

function App() {
  const [lang, setLangState] = useState<Lang>(detectLang);
  const [config, setConfig] = useState<AppConfig>();
  const [failed, setFailed] = useState(false);
  const route = useRoute();

  const i18n = useMemo(
    () => ({
      lang,
      t: STRINGS[lang],
      setLang: (next: Lang) => {
        saveLang(next);
        setLangState(next);
      },
    }),
    [lang],
  );

  useEffect(() => {
    document.documentElement.lang = lang;
  }, [lang]);

  useEffect(() => {
    api
      .config()
      .then(setConfig)
      .catch(() => setFailed(true));
  }, []);

  let body;
  if (failed) body = <Center title={i18n.t.errorGeneric} />;
  else if (!config) body = <Center>{<span className="spinner" />}</Center>;
  else if (route.name === "room") {
    const state = history.state as { autoJoin?: boolean } | null;
    body = <Room key={route.slug} slug={route.slug} autoJoin={state?.autoJoin === true} />;
  } else body = <Lobby stage={config.stage} maxName={config.maxNameLength} />;

  return <I18nContext.Provider value={i18n}>{body}</I18nContext.Provider>;
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
