import { useEffect, useState } from "react";

export type Route = { name: "lobby" } | { name: "room"; slug: string };

export function parseRoute(pathname: string): Route {
  const m = /^\/r\/([a-z0-9-]+)\/?$/.exec(pathname);
  return m && m[1] ? { name: "room", slug: m[1] } : { name: "lobby" };
}

export function roomPath(slug: string): string {
  return `/r/${slug}`;
}

export function navigate(path: string, state: unknown = null): void {
  if (location.pathname !== path) history.pushState(state, "", path);
  else history.replaceState(state, "", path);
  dispatchEvent(new PopStateEvent("popstate"));
}

export function useRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseRoute(location.pathname));
  useEffect(() => {
    const onPop = () => setRoute(parseRoute(location.pathname));
    addEventListener("popstate", onPop);
    return () => removeEventListener("popstate", onPop);
  }, []);
  return route;
}
