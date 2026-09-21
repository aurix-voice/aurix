import { useMutation, useQuery, useQueryClient, type UseMutationResult, type UseQueryResult } from "@tanstack/react-query";
import { useMemo } from "react";

import { useAuth } from "@/auth/AuthProvider";
import type { AppPermission } from "@/auth/store";

import { makeClient, type AurixClient, type T } from "./client";
import { useAppScope } from "./scope";

/** Client for admin-only routes (no tenant header). */
export function useAdminApi(): AurixClient {
  const { token } = useAuth();
  return useMemo(() => makeClient(token, null), [token]);
}

/** Client for tenant routes; delegates via `X-Aurix-App` to the selected application. */
export function useApi(): AurixClient {
  const { token } = useAuth();
  const { appId } = useAppScope();
  return useMemo(() => makeClient(token, appId), [token, appId]);
}

/** Client for tenant routes of an explicit application (app detail pages), independent of the selected scope. */
export function useAppApi(appId: string): AurixClient {
  const { token } = useAuth();
  return useMemo(() => makeClient(token, appId), [token, appId]);
}

/** Query keys. Tenant keys are prefixed with the app id so switching apps never mixes caches. */
export const qk = {
  authMethods: ["auth", "methods"] as const,
  me: ["me"] as const,
  apps: ["apps"] as const,
  app: (id: string) => ["apps", id] as const,
  appKeys: (id: string) => ["apps", id, "keys"] as const,
  appUsage: (id: string) => ["apps", id, "usage"] as const,
  nodes: ["nodes"] as const,
  config: ["config"] as const,
  admins: ["admins"] as const,
  audit: (params: Record<string, unknown>) => ["audit", params] as const,
  adminUsage: (params: Record<string, unknown>) => ["admin", "usage", params] as const,
  adminSessions: (params: Record<string, unknown>) => ["admin", "sessions", params] as const,
  moderationEventsAdmin: (params: Record<string, unknown>) => ["admin", "moderation", params] as const,
  tenant: (app: string) => ["t", app] as const,
  channels: (app: string) => ["t", app, "channels"] as const,
  channel: (app: string, id: string) => ["t", app, "channels", id] as const,
  channelParticipants: (app: string, id: string) => ["t", app, "channels", id, "participants"] as const,
  channelRecordings: (app: string, id: string) => ["t", app, "channels", id, "recordings"] as const,
  sessions: (app: string) => ["t", app, "sessions"] as const,
  sessionStats: (app: string, id: string) => ["t", app, "sessions", id, "stats"] as const,
  users: (app: string, params: Record<string, unknown>) => ["t", app, "users", params] as const,
  user: (app: string, id: string) => ["t", app, "users", id] as const,
  userBlocks: (app: string, id: string) => ["t", app, "users", id, "blocks"] as const,
  userRisk: (app: string, id: string) => ["t", app, "users", id, "risk"] as const,
  moderationEvents: (app: string, params: Record<string, unknown>) => ["t", app, "moderation", params] as const,
  bans: (app: string, params: Record<string, unknown>) => ["t", app, "bans", params] as const,
  incidents: (app: string, params: Record<string, unknown>) => ["t", app, "incidents", params] as const,
  chat: (app: string, params: Record<string, unknown>) => ["t", app, "chat", params] as const,
  recordings: (app: string, params: Record<string, unknown>) => ["t", app, "recordings", params] as const,
  recording: (app: string, id: string) => ["t", app, "recordings", id] as const,
  transcript: (app: string, id: string) => ["t", app, "recordings", id, "transcript"] as const,
  webhooks: (app: string) => ["t", app, "webhooks"] as const,
  webhookDeliveries: (app: string, id: string, params: Record<string, unknown>) =>
    ["t", app, "webhooks", id, "deliveries", params] as const,
  analytics: (app: string, params: Record<string, unknown>) => ["t", app, "analytics", params] as const,
  quota: (app: string) => ["t", app, "quota"] as const,
  regions: ["regions"] as const,
};

export function useAppsQuery(enabled = true): UseQueryResult<T.App[]> {
  const api = useAdminApi();
  const { can, status } = useAuth();
  return useQuery({
    queryKey: qk.apps,
    queryFn: async () => (await api.listApps({ page: 1, per_page: 200 })).data,
    enabled: enabled && status === "authenticated" && can("apps:read"),
    staleTime: 30_000,
  });
}

export function useSelectedApp(): T.App | null {
  const { appId } = useAppScope();
  const apps = useAppsQuery();
  return useMemo(() => apps.data?.find((a) => a.id === appId) ?? null, [apps.data, appId]);
}

export function useAppQuery(appId: string): UseQueryResult<T.AppDetail> {
  const api = useAdminApi();
  const { can } = useAuth();
  return useQuery({
    queryKey: qk.app(appId),
    queryFn: () => api.getApp(appId),
    enabled: can("apps:read"),
    staleTime: 15_000,
  });
}

function invalidateApps(qc: ReturnType<typeof useQueryClient>, appId?: string): Promise<unknown> {
  return Promise.all([
    qc.invalidateQueries({ queryKey: qk.apps }),
    appId ? qc.invalidateQueries({ queryKey: qk.app(appId) }) : Promise.resolve(),
  ]);
}

export function useCreateAppMutation(): UseMutationResult<T.CreatedApp, unknown, T.CreateAppRequest> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({ mutationFn: (body) => api.createApp(body), onSuccess: () => invalidateApps(qc) });
}

export function useUpdateAppMutation(appId: string): UseMutationResult<T.App, unknown, T.UpdateAppRequest> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({ mutationFn: (body) => api.updateApp(appId, body), onSuccess: () => invalidateApps(qc, appId) });
}

export function useDeleteAppMutation(): UseMutationResult<T.DeleteAppResponse, unknown, { appId: string }> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ appId }) => api.deleteApp(appId),
    onSuccess: (_r, { appId }) => {
      qc.removeQueries({ queryKey: qk.tenant(appId) });
      return invalidateApps(qc, appId);
    },
  });
}

export function useRotateAppKeyMutation(appId: string): UseMutationResult<T.RotateAppKeyResponse, unknown, void> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.rotateAppKey(appId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.appKeys(appId) }),
  });
}

export function useAdminAppUsageQuery(appId: string, range: RangeParams & { step: number }): UseQueryResult<T.AdminAppUsage> {
  const api = useAdminApi();
  const { can } = useAuth();
  return useQuery({
    queryKey: [...qk.appUsage(appId), { ...range }],
    queryFn: () => api.adminAppUsage(appId, range),
    enabled: can("analytics:read"),
    staleTime: 60_000,
  });
}

// ---- API keys of an explicit application (admin delegation, `keys:manage`)

export function useAppKeysQuery(appId: string): UseQueryResult<T.ApiKey[]> {
  const api = useAppApi(appId);
  const { status, canApp } = useAuth();
  return useQuery({
    queryKey: qk.appKeys(appId),
    queryFn: () => api.listApiKeys(),
    enabled: status === "authenticated" && canApp("keys:manage"),
    staleTime: 15_000,
  });
}

export function useCreateApiKeyMutation(appId: string): UseMutationResult<T.CreatedApiKey, unknown, T.CreateApiKeyRequest> {
  const api = useAppApi(appId);
  const qc = useQueryClient();
  return useMutation({ mutationFn: (body) => api.createApiKey(body), onSuccess: () => qc.invalidateQueries({ queryKey: qk.appKeys(appId) }) });
}

export function useUpdateApiKeyMutation(appId: string): UseMutationResult<T.ApiKey, unknown, { keyId: string; body: T.UpdateApiKeyRequest }> {
  const api = useAppApi(appId);
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ keyId, body }) => api.updateApiKey(keyId, body),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.appKeys(appId) }),
  });
}

export function useRevokeApiKeyMutation(appId: string): UseMutationResult<T.RevokeApiKeyResponse, unknown, { keyId: string }> {
  const api = useAppApi(appId);
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ keyId }) => api.revokeApiKey(keyId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.appKeys(appId) }),
  });
}

// ---- Webhooks of an explicit application

export function useAppWebhooksQuery(appId: string): UseQueryResult<T.ListWebhooksResponse> {
  const api = useAppApi(appId);
  const { status, canApp } = useAuth();
  return useQuery({
    queryKey: qk.webhooks(appId),
    queryFn: () => api.listWebhooks(),
    enabled: status === "authenticated" && canApp("webhooks:read"),
    staleTime: 15_000,
  });
}

export function useEventTypesQuery(appId: string): UseQueryResult<T.EventTypes> {
  const api = useAppApi(appId);
  const { status, canApp } = useAuth();
  return useQuery({
    queryKey: [...qk.tenant(appId), "event-types"],
    queryFn: () => api.listEventTypes(),
    enabled: status === "authenticated" && canApp("webhooks:read"),
    staleTime: Infinity,
  });
}

export function useWebhookDeliveriesQuery(appId: string, webhookId: string | null, params: T.ListWebhookDeliveriesQuery): UseQueryResult<T.ListWebhookDeliveriesResponse> {
  const api = useAppApi(appId);
  const { status, canApp } = useAuth();
  return useQuery({
    queryKey: qk.webhookDeliveries(appId, webhookId ?? "", { ...params }),
    queryFn: () => api.listWebhookDeliveries(webhookId ?? "", params),
    enabled: status === "authenticated" && !!webhookId && canApp("webhooks:read"),
    refetchInterval: 15_000,
  });
}

function useWebhookMutation<Result, Vars>(appId: string, fn: (api: AurixClient, vars: Vars) => Promise<Result>): UseMutationResult<Result, unknown, Vars> {
  const api = useAppApi(appId);
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: Vars) => fn(api, vars),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(appId) }),
  });
}

export function useCreateWebhookMutation(appId: string) {
  return useWebhookMutation(appId, (api, body: T.CreateWebhookRequest) => api.createWebhook(body));
}
export function useUpdateWebhookMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string; body: T.UpdateWebhookRequest }) => api.updateWebhook(v.webhookId, v.body));
}
export function useDeleteWebhookMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string }) => api.deleteWebhook(v.webhookId));
}
export function useRotateWebhookSecretMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string }) => api.rotateWebhookSecret(v.webhookId));
}
export function useTestWebhookMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string }) => api.testWebhook(v.webhookId));
}
export function useResyncWebhookMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string }) => api.resyncWebhook(v.webhookId));
}
export function useRetryDeliveryMutation(appId: string) {
  return useWebhookMutation(appId, (api, v: { webhookId: string; deliveryId: string }) => api.retryWebhookDelivery(v.webhookId, v.deliveryId));
}

export function useNodesQuery(refetchInterval: number | false = 15_000): UseQueryResult<T.MediaNode[]> {
  const api = useAdminApi();
  const { can } = useAuth();
  return useQuery({
    queryKey: qk.nodes,
    queryFn: () => api.listNodes(),
    enabled: can("nodes:read"),
    refetchInterval,
  });
}

export function useRegionsQuery(): UseQueryResult<T.RegionsResponse> {
  const api = useApi();
  const { appId } = useAppScope();
  const { canApp } = useAuth();
  return useQuery({
    queryKey: qk.regions,
    queryFn: () => api.listRegions(),
    enabled: !!appId && canApp("channels:read"),
    staleTime: 60_000,
  });
}

export function useEffectiveConfigQuery(): UseQueryResult<T.EffectiveConfig> {
  const api = useAdminApi();
  const { can } = useAuth();
  return useQuery({
    queryKey: qk.config,
    queryFn: () => api.adminEffectiveConfig(),
    enabled: can("config:read"),
    staleTime: 5 * 60_000,
  });
}

export function useDrainNodeMutation(): UseMutationResult<T.MediaNode, unknown, { nodeId: string; reason: string | null }> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ nodeId, reason }) => api.drainNode(nodeId, { reason }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.nodes }),
  });
}

export function useUndrainNodeMutation(): UseMutationResult<T.MediaNode, unknown, { nodeId: string }> {
  const api = useAdminApi();
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ nodeId }) => api.undrainNode(nodeId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.nodes }),
  });
}

export interface RangeParams {
  from: string;
  to: string;
}

export function useFleetUsageQuery(range: RangeParams): UseQueryResult<T.FleetUsage> {
  const api = useAdminApi();
  const { can } = useAuth();
  return useQuery({
    queryKey: qk.adminUsage({ ...range }),
    queryFn: () => api.adminUsage(range),
    enabled: can("analytics:read"),
    staleTime: 60_000,
  });
}

/** Tenant-scoped query gate: an app is selected and the role delegates the permission. */
export function useTenantEnabled(perm: AppPermission): { appId: string | null; enabled: boolean } {
  const { appId } = useAppScope();
  const { status, canApp } = useAuth();
  return { appId, enabled: status === "authenticated" && !!appId && canApp(perm) };
}

export function useAppAnalyticsQuery(range: RangeParams & { step: number }): UseQueryResult<T.AppUsage> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("analytics:read");
  return useQuery({
    queryKey: qk.analytics(appId ?? "", { ...range }),
    queryFn: () => api.getAnalytics(range),
    enabled,
    staleTime: 60_000,
  });
}

export function useQuotaQuery(): UseQueryResult<T.QuotaState> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("analytics:read");
  return useQuery({
    queryKey: qk.quota(appId ?? ""),
    queryFn: () => api.getQuota(),
    enabled,
    refetchInterval: 30_000,
  });
}

export function useEventSnapshotQuery(refetchInterval: number | false = 15_000): UseQueryResult<T.EventSnapshot> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("events:read");
  return useQuery({
    queryKey: qk.sessions(appId ?? ""),
    queryFn: () => api.eventSnapshot(),
    enabled,
    refetchInterval,
  });
}

export function useModerationEventsQuery(params: T.ListModerationEventsQuery): UseQueryResult<T.ModerationEvent[]> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("moderation:read");
  return useQuery({
    queryKey: qk.moderationEvents(appId ?? "", { ...params }),
    queryFn: () => api.listModerationEvents(params),
    enabled,
    staleTime: 15_000,
  });
}

export function useSessionQualityQuery(params: T.ListSessionQualityQuery): UseQueryResult<T.SessionQualityList> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("analytics:read");
  return useQuery({
    queryKey: qk.adminSessions({ app: appId, ...params }),
    queryFn: () => api.listSessionQuality(params),
    enabled,
    staleTime: 60_000,
  });
}

export function useWebhooksQuery(): UseQueryResult<T.ListWebhooksResponse> {
  const api = useApi();
  const { appId, enabled } = useTenantEnabled("webhooks:read");
  return useQuery({
    queryKey: qk.webhooks(appId ?? ""),
    queryFn: () => api.listWebhooks(),
    enabled,
    staleTime: 30_000,
  });
}
