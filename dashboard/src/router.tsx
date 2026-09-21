import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  redirect,
} from "@tanstack/react-router";
import { lazy, Suspense } from "react";

import { authStore } from "@/auth/store";
import { AuthLayout } from "@/auth/AuthLayout";
import { CallbackPage } from "@/auth/CallbackPage";
import { LoginPage } from "@/auth/LoginPage";
import { SetupPage } from "@/auth/SetupPage";
import { Shell } from "@/shell/Shell";
import { NotFound } from "@/shell/NotFound";
import { Loading } from "@/ui/Page";

const OverviewPage = lazy(() => import("@/pages/overview/OverviewPage"));
const NodesPage = lazy(() => import("@/pages/nodes/NodesPage"));
const AppsPage = lazy(() => import("@/pages/apps/AppsPage"));
const AppDetailPage = lazy(() => import("@/pages/apps/AppDetailPage"));
const LivePage = lazy(() => import("@/pages/live/LivePage"));
const ChannelPage = lazy(() => import("@/pages/live/ChannelPage"));
const ModerationPage = lazy(() => import("@/pages/moderation/ModerationPage"));
const RecordingsPage = lazy(() => import("@/pages/recordings/RecordingsPage"));
const AnalyticsPage = lazy(() => import("@/pages/analytics/AnalyticsPage"));
const AdminPage = lazy(() => import("@/pages/admin/AdminPage"));
const ConfigPage = lazy(() => import("@/pages/config/ConfigPage"));

function page(Component: React.LazyExoticComponent<() => React.JSX.Element>) {
  return function Page() {
    return (
      <Suspense fallback={<Loading />}>
        <Component />
      </Suspense>
    );
  };
}

const rootRoute = createRootRoute({
  component: Outlet,
  notFoundComponent: NotFound,
});

// ---- Public (auth) routes
const authRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "auth",
  component: AuthLayout,
});

export interface LoginSearch {
  redirect?: string;
}

const loginRoute = createRoute({
  getParentRoute: () => authRoute,
  path: "/login",
  validateSearch: (s: Record<string, unknown>): LoginSearch => ({
    redirect: typeof s.redirect === "string" && s.redirect.startsWith("/") ? s.redirect : undefined,
  }),
  beforeLoad: ({ search }) => {
    if (authStore.get()) throw redirect({ to: search.redirect ?? "/" });
  },
  component: LoginPage,
});

const setupRoute = createRoute({
  getParentRoute: () => authRoute,
  path: "/setup",
  component: SetupPage,
});

const callbackRoute = createRoute({
  getParentRoute: () => authRoute,
  path: "/auth/callback",
  component: CallbackPage,
});

// ---- Protected shell
const shellRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "shell",
  beforeLoad: ({ location }) => {
    if (!authStore.get()) {
      throw redirect({ to: "/login", search: { redirect: location.href === "/" ? undefined : location.href } });
    }
  },
  component: Shell,
});

export const overviewRoute = createRoute({ getParentRoute: () => shellRoute, path: "/", component: page(OverviewPage) });
export const nodesRoute = createRoute({ getParentRoute: () => shellRoute, path: "/nodes", component: page(NodesPage) });
export const appsRoute = createRoute({ getParentRoute: () => shellRoute, path: "/apps", component: page(AppsPage) });
export const appDetailRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/apps/$appId",
  validateSearch: (s: Record<string, unknown>): { tab?: string } => ({ tab: typeof s.tab === "string" ? s.tab : undefined }),
  component: page(AppDetailPage),
});
export const liveRoute = createRoute({ getParentRoute: () => shellRoute, path: "/live", component: page(LivePage) });
export const channelRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/live/$channelId",
  validateSearch: (s: Record<string, unknown>): { tab?: string; session?: string } => ({
    tab: typeof s.tab === "string" ? s.tab : undefined,
    session: typeof s.session === "string" ? s.session : undefined,
  }),
  component: page(ChannelPage),
});
export const moderationRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/moderation",
  validateSearch: (s: Record<string, unknown>): { tab?: string; user?: string; channel?: string; event?: string } => ({
    tab: typeof s.tab === "string" ? s.tab : undefined,
    user: typeof s.user === "string" ? s.user : undefined,
    channel: typeof s.channel === "string" ? s.channel : undefined,
    event: typeof s.event === "string" ? s.event : undefined,
  }),
  component: page(ModerationPage),
});
export const recordingsRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/recordings",
  validateSearch: (s: Record<string, unknown>): { id?: string; channel?: string } => ({
    id: typeof s.id === "string" ? s.id : undefined,
    channel: typeof s.channel === "string" ? s.channel : undefined,
  }),
  component: page(RecordingsPage),
});
export const analyticsRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/analytics",
  validateSearch: (s: Record<string, unknown>): { tab?: string } => ({ tab: typeof s.tab === "string" ? s.tab : undefined }),
  component: page(AnalyticsPage),
});
export const adminRoute = createRoute({
  getParentRoute: () => shellRoute,
  path: "/settings",
  validateSearch: (s: Record<string, unknown>): { tab?: string } => ({ tab: typeof s.tab === "string" ? s.tab : undefined }),
  component: page(AdminPage),
});
export const configRoute = createRoute({ getParentRoute: () => shellRoute, path: "/config", component: page(ConfigPage) });

const routeTree = rootRoute.addChildren([
  authRoute.addChildren([loginRoute, setupRoute, callbackRoute]),
  shellRoute.addChildren([
    overviewRoute,
    nodesRoute,
    appsRoute,
    appDetailRoute,
    liveRoute,
    channelRoute,
    moderationRoute,
    recordingsRoute,
    analyticsRoute,
    adminRoute,
    configRoute,
  ]),
]);

export const router = createRouter({
  routeTree,
  defaultPreload: "intent",
  scrollRestoration: true,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
