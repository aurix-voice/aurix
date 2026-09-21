import { Link, useNavigate } from "@tanstack/react-router";
import { ArrowLeft, Ban, Mic, MicOff, RefreshCw, Settings2, Star, StarOff, UserX, Users } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";

import { errorKind, errorMessage, type T } from "@/api/client";
import {
  useChannelParticipantsQuery,
  useChannelQuery,
  useChannelStreamsQuery,
  useDeleteStreamMutation,
  useKickMutation,
  useServerMuteMutation,
  useSessionStatsQuery,
  useSetPriorityMutation,
  useUpdateChannelConfigMutation,
} from "@/api/hooks";
import { useAuth } from "@/auth/AuthProvider";
import { MosBadge, TimeSeriesChart, chart, type Point } from "@/components/charts";
import { EventFeed } from "@/components/EventFeed";
import { useI18n } from "@/i18n";
import { fmtBytes, fmtDateTime, fmtMos, fmtNumber, fmtPercent, fmtRelative, shortId } from "@/lib/format";
import { useNow } from "@/lib/useNow";
import { BanDialog } from "@/pages/moderation/BanDialog";
import { channelRoute } from "@/router";
import { RequireApp } from "@/shell/Guards";
import { Button } from "@/ui/Button";
import { ConfirmDialog, FormDialog } from "@/ui/Dialog";
import { Input } from "@/ui/Input";
import { Menu, TabPanel, Tabs, type MenuItem } from "@/ui/Menu";
import { PageHeader, QueryError, SplitLayout } from "@/ui/Page";
import { Badge, Callout, Card, CardHeader, EmptyState, Field, IdChip, KV, Stat, Tip } from "@/ui/Primitives";
import { DataTable, type Column } from "@/ui/Table";
import { useToast } from "@/ui/Toast";

import { ChannelFlags, ChannelTypeBadge } from "./ChannelBadges";
import { ChannelActionDialogs, useChannelMenuItems, type ChannelAction } from "./ChannelActions";
import { ChannelConfigForm, configProblems, parseConfigJson, type ConfigMode } from "./ChannelConfigForm";

type TabId = "participants" | "config" | "streams" | "events";
const TABS: readonly TabId[] = ["participants", "config", "streams", "events"];

export default function ChannelPage() {
  return <RequireApp>{() => <ChannelDetail />}</RequireApp>;
}

/** Membership row (DB roster) joined with the media-plane view of this node when the session lives here. */
interface ParticipantRow {
  key: string;
  session_id: string;
  user_id: string;
  display_name: string;
  role?: string;
  is_muted: boolean;
  is_server_muted: boolean;
  is_speaking?: boolean;
  transport?: string;
  quality?: T.NetworkQuality | null;
  joined_at?: string;
  live: boolean;
  node_id?: string | null;
}

function mergeParticipants(p: T.ChannelParticipants | undefined): ParticipantRow[] {
  if (!p) return [];
  const live = new Map(p.live_on_this_node.map((l) => [l.session_id, l]));
  const rows: ParticipantRow[] = p.memberships.map((m) => {
    const l = live.get(m.session_id);
    live.delete(m.session_id);
    return {
      key: m.session_id,
      session_id: m.session_id,
      user_id: m.user_id,
      display_name: m.display_name ?? l?.display_name ?? "",
      role: m.role,
      is_muted: Boolean(l?.is_muted ?? m.is_muted),
      is_server_muted: Boolean(l?.is_server_muted ?? m.is_server_muted),
      is_speaking: l?.is_speaking,
      transport: l?.transport,
      quality: l?.quality,
      joined_at: m.joined_at,
      live: Boolean(l),
      node_id: m.media_node_id,
    };
  });
  for (const l of live.values()) {
    rows.push({
      key: l.session_id,
      session_id: l.session_id,
      user_id: l.user_id,
      display_name: l.display_name ?? "",
      is_muted: Boolean(l.is_muted),
      is_server_muted: Boolean(l.is_server_muted),
      is_speaking: l.is_speaking,
      transport: l.transport,
      quality: l.quality,
      live: true,
    });
  }
  return rows;
}

type UserAction = "mute" | "unmute" | "kick" | "ban" | "priorityOn" | "priorityOff";

function ChannelDetail() {
  const { channelId } = channelRoute.useParams();
  const search = channelRoute.useSearch();
  const navigate = useNavigate();
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const now = useNow(5_000);

  const tab: TabId = TABS.includes(search.tab as TabId) ? (search.tab as TabId) : "participants";
  const selected = search.session ?? null;
  const setSearch = (patch: { tab?: TabId; session?: string | null }) =>
    void navigate({
      to: "/live/$channelId",
      params: { channelId },
      search: { tab: patch.tab ?? tab, session: patch.session === undefined ? (selected ?? undefined) : (patch.session ?? undefined) },
      replace: true,
    });

  const channel = useChannelQuery(channelId);
  const participants = useChannelParticipantsQuery(channelId);
  const rows = useMemo(() => mergeParticipants(participants.data), [participants.data]);

  const [channelAction, setChannelAction] = useState<ChannelAction | null>(null);
  const [userAction, setUserAction] = useState<{ row: ParticipantRow; action: UserAction } | null>(null);
  const [editing, setEditing] = useState(false);
  const menuItems = useChannelMenuItems(channel.data ?? { id: channelId, name: "" }, setChannelAction);

  if (!canApp("channels:read")) {
    return <EmptyState title={t("common.forbidden")} description={t("common.forbidden.perm", { perm: "channels:read" })} />;
  }
  if (channel.isError) {
    const kind = errorKind(channel.error);
    return (
      <>
        <BackLink />
        {kind === "notFound" ? (
          <EmptyState title={t("live.notFound")} description={t("live.notFound.desc")} action={<BackButton />} />
        ) : (
          <QueryError error={channel.error} onRetry={() => void channel.refetch()} />
        )}
      </>
    );
  }

  const c = channel.data;
  const liveCount = participants.data?.live_on_this_node.length ?? 0;
  const speaking = rows.filter((r) => r.is_speaking).length;
  const muted = rows.filter((r) => r.is_muted || r.is_server_muted).length;
  const canModerate = canApp("moderation:write");
  const canStreams = canApp("audio_streams:read");

  return (
    <>
      <BackLink />
      <PageHeader
        title={
          c ? (
            <span className="inline-flex items-center gap-2 flex-wrap">
              {c.name}
              <ChannelTypeBadge type={c.channel_type} />
              {c.deleted_at ? <Badge tone="danger">{t("common.deleted")}</Badge> : null}
            </span>
          ) : (
            <span className="inline-block h-6 w-48 rounded bg-surface-2 animate-pulse" />
          )
        }
        description={
          <span className="inline-flex items-center gap-3 flex-wrap">
            <IdChip id={channelId} />
            {c ? <ChannelFlags config={c.config} adHoc={c.ad_hoc} persistent={c.is_persistent} /> : null}
          </span>
        }
        actions={
          <>
            <Button variant="ghost" size="sm" onClick={() => void Promise.all([channel.refetch(), participants.refetch()])} loading={participants.isFetching && !participants.isPending}>
              <RefreshCw className="size-3.5" />
              {t("common.refresh")}
            </Button>
            {canApp("channels:write") && c ? (
              <Button variant="secondary" size="sm" onClick={() => setEditing(true)} data-testid="edit-config">
                <Settings2 className="size-3.5" />
                {t("live.channelConfig")}
              </Button>
            ) : null}
            {menuItems.length ? <Menu items={menuItems} /> : null}
          </>
        }
      />

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4 mb-4">
        <Stat
          label={t("live.participants")}
          value={participants.isPending ? "…" : `${fmtNumber(locale, rows.length)} / ${c ? fmtNumber(locale, c.max_participants) : "—"}`}
          hint={t("live.liveOnNode", { n: liveCount })}
        />
        <Stat label={t("live.speaking")} value={participants.isPending ? "…" : fmtNumber(locale, speaking)} hint={t("live.speaking.hint")} />
        <Stat label={t("live.muted")} value={participants.isPending ? "…" : fmtNumber(locale, muted)} hint={t("live.muted.hint")} />
        <Stat label={t("live.bitrate")} value={c?.config.bitrate ? `${Math.round(c.config.bitrate / 1000)} kbps` : "—"} hint={c?.config.audio_profile ? String(c.config.audio_profile) : t("live.audioProfile")} />
      </div>

      <Tabs
        value={tab}
        onValueChange={(v) => setSearch({ tab: v as TabId })}
        tabs={[
          { value: "participants", label: t("live.participants"), count: rows.length || undefined },
          { value: "config", label: t("live.channelConfig") },
          { value: "streams", label: t("live.streams"), hidden: !canStreams },
          { value: "events", label: t("live.events") },
        ]}
      >
        <TabPanel value="participants" className="pt-4 outline-none">
          <SplitLayout
            main={
              <Card>
                <ParticipantsTable
                  rows={rows}
                  loading={participants.isPending}
                  error={participants.isError ? <QueryError error={participants.error} onRetry={() => void participants.refetch()} compact /> : undefined}
                  selected={selected}
                  onSelect={(sid) => setSearch({ session: sid })}
                  canModerate={canModerate}
                  onAction={(row, action) => setUserAction({ row, action })}
                  now={now}
                />
              </Card>
            }
            side={<SessionPanel sessionId={selected} row={rows.find((r) => r.session_id === selected)} onClose={() => setSearch({ session: null })} />}
          />
        </TabPanel>
        <TabPanel value="config" className="pt-4 outline-none">
          {c ? <ConfigView channel={c} onEdit={canApp("channels:write") ? () => setEditing(true) : undefined} /> : null}
        </TabPanel>
        {canStreams ? (
          <TabPanel value="streams" className="pt-4 outline-none">
            <StreamsTab channelId={channelId} />
          </TabPanel>
        ) : null}
        <TabPanel value="events" className="pt-4 outline-none">
          <EventFeed channelId={channelId} height={520} />
        </TabPanel>
      </Tabs>

      <ChannelActionDialogs
        channel={c ?? null}
        action={channelAction}
        onClose={() => setChannelAction(null)}
        onDeleted={() => void navigate({ to: "/live", replace: true })}
      />
      <UserActionDialog target={userAction} channelId={channelId} onClose={() => setUserAction(null)} />
      {c ? <EditConfigDialog open={editing} onOpenChange={setEditing} channel={c} /> : null}
    </>
  );
}

function BackLink() {
  const { t } = useI18n();
  return (
    <Link to="/live" className="inline-flex items-center gap-1 text-xs text-fg-muted hover:text-fg mb-2">
      <ArrowLeft className="size-3.5" />
      {t("live.title")}
    </Link>
  );
}

function BackButton() {
  const { t } = useI18n();
  return (
    <Link to="/live">
      <Button variant="secondary" size="sm">
        {t("live.backToList")}
      </Button>
    </Link>
  );
}

function SignalBars({ bars }: { bars: number }) {
  return (
    <span className="inline-flex items-end gap-px h-3" aria-label={`${bars}/5`}>
      {[1, 2, 3, 4, 5].map((i) => (
        <span key={i} className={`w-[3px] rounded-[1px] ${i <= bars ? (bars >= 4 ? "bg-ok" : bars >= 3 ? "bg-fg-muted" : "bg-warn") : "bg-border"}`} style={{ height: `${4 + i * 1.6}px` }} />
      ))}
    </span>
  );
}

function ParticipantsTable({
  rows,
  loading,
  error,
  selected,
  onSelect,
  canModerate,
  onAction,
  now,
}: {
  rows: ParticipantRow[];
  loading: boolean;
  error?: ReactNode;
  selected: string | null;
  onSelect: (sessionId: string) => void;
  canModerate: boolean;
  onAction: (row: ParticipantRow, a: UserAction) => void;
  now: number;
}) {
  const { t, locale } = useI18n();
  const columns: Column<ParticipantRow>[] = [
    {
      key: "user",
      header: t("live.user"),
      sort: (r) => r.display_name || r.user_id,
      cell: (r) => (
        <div className="flex items-center gap-2 min-w-0">
          <span className={`size-2 rounded-full shrink-0 ${r.is_speaking ? "bg-ok animate-pulse" : r.live ? "bg-fg-faint" : "bg-border"}`} />
          <div className="flex flex-col min-w-0">
            <span className="font-medium truncate">{r.display_name || <span className="text-fg-faint">{t("live.noName")}</span>}</span>
            <IdChip id={r.user_id} />
          </div>
        </div>
      ),
    },
    {
      key: "state",
      header: t("common.status"),
      cell: (r) => (
        <span className="inline-flex gap-1 flex-wrap">
          {r.is_server_muted ? <Badge tone="danger">{t("live.serverMuted")}</Badge> : r.is_muted ? <Badge tone="neutral">{t("live.selfMuted")}</Badge> : null}
          {r.is_speaking ? <Badge tone="ok">{t("live.speakingNow")}</Badge> : null}
          {r.role && r.role !== "speaker" ? <Badge tone="neutral">{r.role}</Badge> : null}
          {!r.live ? (
            <Tip content={r.node_id ? `${t("live.otherNode.hint")} (${r.node_id})` : t("live.otherNode.hint")}>
              <span>
                <Badge tone="neutral">
                  {t("live.otherNode")}
                  {r.node_id ? <span className="font-mono ml-1 opacity-70">{shortId(r.node_id)}</span> : null}
                </Badge>
              </span>
            </Tip>
          ) : null}
        </span>
      ),
    },
    { key: "transport", header: t("live.transport"), sort: (r) => r.transport ?? "", cell: (r) => <span className="text-fg-muted">{r.transport ?? "—"}</span> },
    {
      key: "quality",
      header: t("live.mos"),
      align: "right",
      sort: (r) => r.quality?.mos ?? -1,
      cell: (r) =>
        r.quality ? (
          <span className="inline-flex items-center gap-2 justify-end">
            <SignalBars bars={r.quality.bars} />
            <MosBadge mos={r.quality.mos} />
          </span>
        ) : (
          <span className="text-fg-faint">—</span>
        ),
    },
    {
      key: "joined",
      header: t("live.joined"),
      sort: (r) => r.joined_at ?? "",
      cell: (r) => <span className="text-fg-muted">{r.joined_at ? fmtRelative(locale, r.joined_at, now) : "—"}</span>,
    },
    {
      key: "actions",
      header: "",
      align: "right",
      width: "3rem",
      cell: (r) =>
        canModerate ? (
          <span onClick={(e) => e.stopPropagation()}>
            <Menu
              items={
                [
                  r.is_server_muted
                    ? { label: t("live.unmute"), icon: <Mic className="size-3.5" />, onSelect: () => onAction(r, "unmute") }
                    : { label: t("live.mute"), icon: <MicOff className="size-3.5" />, onSelect: () => onAction(r, "mute") },
                  { label: t("live.setPriority"), icon: <Star className="size-3.5" />, onSelect: () => onAction(r, "priorityOn") },
                  { label: t("live.clearPriority"), icon: <StarOff className="size-3.5" />, onSelect: () => onAction(r, "priorityOff") },
                  { label: t("live.kick"), icon: <UserX className="size-3.5" />, onSelect: () => onAction(r, "kick"), danger: true, separatorBefore: true },
                  { label: t("live.ban"), icon: <Ban className="size-3.5" />, onSelect: () => onAction(r, "ban"), danger: true },
                ] satisfies MenuItem[]
              }
            />
          </span>
        ) : null,
    },
  ];
  return (
    <DataTable
      rows={rows}
      columns={columns}
      rowKey={(r) => r.key}
      loading={loading}
      error={error}
      selectedKey={selected ?? undefined}
      onRowClick={(r) => onSelect(r.session_id)}
      empty={<EmptyState compact icon={<Users className="size-5" />} title={t("live.noParticipants")} description={t("live.noParticipants.desc")} />}
      dense
    />
  );
}

const HISTORY_MAX = 300;
type QualityHistory = { session: string | null; lastAt: number; points: Point[] };

function SessionPanel({ sessionId, row, onClose }: { sessionId: string | null; row?: ParticipantRow; onClose: () => void }) {
  const { t, locale } = useI18n();
  const stats = useSessionStatsQuery(sessionId);
  const [history, setHistory] = useState<QualityHistory>({ session: null, lastAt: 0, points: [] });
  const { data: statsData, dataUpdatedAt } = stats;
  if (history.session !== sessionId) {
    setHistory({ session: sessionId, lastAt: 0, points: [] });
  } else if (statsData && dataUpdatedAt > history.lastAt) {
    const q = statsData.quality ?? null;
    const point: Point = {
      t: dataUpdatedAt,
      mos: q?.mos ?? statsData.client_report?.mos_score ?? null,
      rtt: q?.rtt_ms ?? statsData.client_report?.rtt_ms ?? null,
      jitter: q?.downlink_jitter_ms ?? statsData.client_report?.jitter_ms ?? null,
      loss: q?.downlink_loss_percent ?? statsData.client_report?.packet_loss_percent ?? null,
    };
    setHistory({ session: sessionId, lastAt: dataUpdatedAt, points: [...history.points, point].slice(-HISTORY_MAX) });
  }
  const points = history.session === sessionId ? history.points : [];

  if (!sessionId) {
    return (
      <Card className="p-6">
        <EmptyState compact title={t("live.selectSession")} description={t("live.selectSession.desc")} />
      </Card>
    );
  }

  const d = stats.data;
  const q = d?.quality ?? null;
  const s = d?.quality_summary ?? null;
  const title = row?.display_name || d?.user_id || sessionId;

  return (
    <div className="flex flex-col gap-3">
      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-2">
              {title}
              {d?.mos_alerting ? <Badge tone="danger">{t("live.mosAlert")}</Badge> : null}
            </span>
          }
          description={<IdChip id={sessionId} />}
          actions={
            <Button variant="ghost" size="sm" onClick={onClose}>
              {t("common.close")}
            </Button>
          }
        />
        <div className="px-4 pb-4">
          {stats.isError ? (
            errorKind(stats.error) === "notFound" ? (
              <Callout tone="neutral" title={t("live.stats.otherNode")}>
                {t("live.stats.otherNode.desc")}
              </Callout>
            ) : (
              <QueryError error={stats.error} onRetry={() => void stats.refetch()} compact />
            )
          ) : d ? (
            <>
              <div className="grid grid-cols-2 gap-3 mb-4">
                <Stat label={t("live.mos")} value={<span className="inline-flex items-center gap-2">{q ? <SignalBars bars={q.bars} /> : null}<MosBadge mos={q?.mos ?? d.client_report?.mos_score} /></span>} hint={q ? `R ${q.r_factor.toFixed(0)}` : t("live.noQuality")} />
                <Stat label={t("live.rtt")} value={q?.rtt_ms !== undefined ? `${Math.round(q.rtt_ms)} ms` : d.client_report?.rtt_ms !== undefined ? `${Math.round(d.client_report.rtt_ms)} ms` : "—"} hint={t("live.jitter") + ": " + (q?.downlink_jitter_ms !== undefined ? `${q.downlink_jitter_ms.toFixed(1)} ms` : "—")} />
                <Stat label={t("live.loss")} value={fmtPercent(locale, q?.downlink_loss_percent ?? d.client_report?.packet_loss_percent)} hint={t("live.loss.uplink", { v: fmtPercent(locale, q?.uplink_loss_percent) })} />
                <Stat label={t("live.bitrate")} value={q?.uplink_bitrate_kbps !== undefined ? `${Math.round(q.uplink_bitrate_kbps)} kbps` : d.client_report?.bitrate_kbps !== undefined ? `${Math.round(d.client_report.bitrate_kbps)} kbps` : "—"} hint={t("live.bitrate.hint")} />
              </div>
              <KV
                items={[
                  { k: t("live.transport"), v: d.transport },
                  { k: t("live.mediaPath"), v: d.media_path ?? "—" },
                  { k: t("live.channels"), v: fmtNumber(locale, d.channels.length) },
                  { k: t("live.packetsSent"), v: `${fmtNumber(locale, d.packets_sent)} · ${fmtBytes(locale, d.bytes_sent)}` },
                  { k: t("live.packetsReceived"), v: `${fmtNumber(locale, d.packets_received)} · ${fmtBytes(locale, d.bytes_received)}` },
                ]}
              />
            </>
          ) : (
            <div className="h-32 rounded-lg bg-surface-2 animate-pulse" />
          )}
        </div>
      </Card>

      {d ? (
        <Card>
          <CardHeader title={t("live.qualityHistory")} description={t("live.qualityHistory.hint")} />
          <div className="px-2 pb-3">
            <TimeSeriesChart
              data={points}
              height={180}
              leftDomain={[1, 4.6]}
              rightDomain={[0, "auto"]}
              series={[
                { key: "mos", label: t("live.mos"), color: chart.accent, format: fmtMos },
                { key: "rtt", label: t("live.rtt"), color: chart.muted, right: true, format: (v) => `${Math.round(v)} ms` },
                { key: "loss", label: t("live.loss"), color: chart.danger, right: true, format: (v) => fmtPercent(locale, v) },
              ]}
              empty={t("live.qualityHistory.empty")}
            />
          </div>
          {s ? (
            <div className="px-4 pb-4">
              <KV
                items={[
                  { k: t("live.summary.window"), v: t("live.summary.windowValue", { s: fmtNumber(locale, s.seconds), n: fmtNumber(locale, s.samples) }) },
                  { k: t("live.summary.mos"), v: `${fmtMos(s.mos_avg)} · min ${fmtMos(s.mos_min)}` },
                  { k: t("live.summary.rtt"), v: `${Math.round(s.rtt_avg_ms)} / ${Math.round(s.rtt_max_ms)} ms` },
                  { k: t("live.summary.loss"), v: `${fmtPercent(locale, s.loss_avg_percent)} / ${fmtPercent(locale, s.loss_max_percent)}` },
                  { k: t("live.summary.poor"), v: t("live.summary.poorValue", { s: fmtNumber(locale, s.poor_seconds), n: fmtNumber(locale, s.mos_alerts) }) },
                ]}
              />
            </div>
          ) : null}
        </Card>
      ) : null}
    </div>
  );
}

function ConfigView({ channel, onEdit }: { channel: T.Channel; onEdit?: () => void }) {
  const { t, locale } = useI18n();
  const cfg = channel.config;
  const yesNo = (v: boolean | null | undefined) => (v ? t("common.yes") : t("common.no"));
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader
          title={t("live.channelConfig")}
          actions={
            onEdit ? (
              <Button variant="secondary" size="sm" onClick={onEdit}>
                <Settings2 className="size-3.5" />
                {t("common.edit")}
              </Button>
            ) : undefined
          }
        />
        <div className="px-4 pb-4">
          <KV
            items={[
              { k: t("live.channelType"), v: <ChannelTypeBadge type={channel.channel_type} /> },
              { k: t("live.maxParticipants"), v: fmtNumber(locale, channel.max_participants) },
              { k: t("live.audioProfile"), v: cfg.audio_profile ? String(cfg.audio_profile) : "—" },
              { k: t("live.bitrateBps"), v: cfg.bitrate !== undefined ? fmtNumber(locale, cfg.bitrate) : "—" },
              { k: t("live.minBitrateBps"), v: cfg.min_bitrate !== undefined && cfg.min_bitrate !== null ? fmtNumber(locale, cfg.min_bitrate) : "—" },
              { k: t("live.sampleRate"), v: cfg.sample_rate !== undefined ? `${fmtNumber(locale, cfg.sample_rate)} Hz` : "—" },
              { k: t("live.complexity"), v: cfg.complexity !== undefined && cfg.complexity !== null ? String(cfg.complexity) : "—" },
              { k: t("live.dtx"), v: yesNo(cfg.enable_dtx) },
              { k: t("live.fec"), v: yesNo(cfg.enable_fec) },
              { k: t("live.stereo"), v: yesNo(cfg.stereo) },
              { k: t("live.recording"), v: yesNo(cfg.recording_enabled) },
              { k: t("live.transcription"), v: yesNo(cfg.transcription) },
              { k: t("live.safetyVoice"), v: yesNo(cfg.safety_voice) },
              { k: t("live.e2ee"), v: yesNo(cfg.e2ee) },
              { k: t("common.created"), v: fmtDateTime(locale, channel.created_at) },
              { k: t("common.updated"), v: fmtDateTime(locale, channel.updated_at) },
            ]}
          />
        </div>
      </Card>
      <Card>
        <CardHeader title={t("common.json")} description={t("live.config.rawHint")} />
        <pre className="px-4 pb-4 text-[12px] leading-5 font-mono text-fg-muted overflow-auto max-h-[520px] subtle-scroll">{JSON.stringify(cfg, null, 2)}</pre>
      </Card>
    </div>
  );
}

function EditConfigDialog({ open, onOpenChange, channel }: { open: boolean; onOpenChange: (o: boolean) => void; channel: T.Channel }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const update = useUpdateChannelConfigMutation();
  const [config, setConfig] = useState<T.ChannelConfig>(channel.config);
  const [mode, setMode] = useState<ConfigMode>("form");
  const [json, setJson] = useState(() => JSON.stringify(channel.config, null, 2));
  const [jsonError, setJsonError] = useState<string | null>(null);
  const [seed, setSeed] = useState(channel.updated_at);

  if (seed !== channel.updated_at && !open) {
    setSeed(channel.updated_at);
    setConfig(channel.config);
    setJson(JSON.stringify(channel.config, null, 2));
  }

  const switchMode = (m: ConfigMode) => {
    if (m === "json") setJson(JSON.stringify(config, null, 2));
    else {
      const r = parseConfigJson(json);
      if ("error" in r) {
        setJsonError(r.error);
        return;
      }
      setConfig(r.config);
    }
    setJsonError(null);
    setMode(m);
  };

  const submit = async () => {
    let cfg = config;
    if (mode === "json") {
      const r = parseConfigJson(json);
      if ("error" in r) {
        setJsonError(r.error);
        throw new Error(t("live.config.jsonInvalid"));
      }
      cfg = r.config;
    }
    const problems = configProblems(cfg);
    if (problems.length) throw new Error(t("live.config.invalid", { fields: problems.join(", ") }));
    try {
      await update.mutateAsync({ channelId: channel.id, config: cfg });
      toast.ok(t("live.configSaved"));
    } catch (e) {
      throw new Error(errorMessage(e, locale), { cause: e });
    }
  };

  return (
    <FormDialog open={open} onOpenChange={onOpenChange} title={t("live.channelConfig")} description={t("live.config.editDesc", { name: channel.name })} submitLabel={t("common.save")} size="lg" onSubmit={submit}>
      <ChannelConfigForm value={config} onChange={setConfig} mode={mode} onModeChange={switchMode} json={json} onJsonChange={setJson} jsonError={jsonError} showType={false} />
    </FormDialog>
  );
}

function UserActionDialog({ target, channelId, onClose }: { target: { row: ParticipantRow; action: UserAction } | null; channelId: string; onClose: () => void }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const mute = useServerMuteMutation();
  const kick = useKickMutation();
  const priority = useSetPriorityMutation();
  const [reason, setReason] = useState("");

  const open = target !== null;
  const close = () => {
    setReason("");
    onClose();
  };
  const who = target ? target.row.display_name || target.row.user_id : "";
  const wrap = async (fn: () => Promise<unknown>, okKey: Parameters<typeof t>[0]) => {
    try {
      await fn();
      toast.ok(t(okKey));
    } catch (e) {
      throw new Error(errorMessage(e, locale), { cause: e });
    }
  };

  if (!target) return null;
  const { row, action } = target;
  const base = { user_id: row.user_id, channel_id: channelId };

  if (action === "mute" || action === "unmute") {
    const muted = action === "mute";
    return (
      <ConfirmDialog
        open={open}
        onOpenChange={(o) => !o && close()}
        title={t(muted ? "live.mute" : "live.unmute")}
        description={t(muted ? "live.mute.confirm" : "live.unmute.confirm", { who })}
        confirmLabel={t(muted ? "live.mute" : "live.unmute")}
        variant={muted ? "danger" : "primary"}
        onConfirm={() => wrap(() => mute.mutateAsync({ ...base, muted }), muted ? "live.muted.done" : "live.unmuted.done")}
      />
    );
  }
  if (action === "priorityOn" || action === "priorityOff") {
    const on = action === "priorityOn";
    return (
      <ConfirmDialog
        open={open}
        onOpenChange={(o) => !o && close()}
        title={t(on ? "live.setPriority" : "live.clearPriority")}
        description={t(on ? "live.setPriority.confirm" : "live.clearPriority.confirm", { who })}
        confirmLabel={t(on ? "live.setPriority" : "live.clearPriority")}
        onConfirm={() => wrap(() => priority.mutateAsync({ ...base, priority: on }), on ? "live.priority.done" : "live.priority.cleared")}
      />
    );
  }
  if (action === "kick") {
    return (
      <ConfirmDialog
        open={open}
        onOpenChange={(o) => !o && close()}
        title={t("live.kick")}
        description={t("live.kick.confirm", { who })}
        confirmLabel={t("live.kick")}
        variant="danger"
        disabled={!reason.trim()}
        onConfirm={() => wrap(() => kick.mutateAsync({ ...base, reason: reason.trim() }), "live.kicked.done")}
      >
        <Field label={t("live.reason")} required>
          <Input value={reason} onChange={(e) => setReason(e.target.value)} maxLength={256} autoFocus />
        </Field>
      </ConfirmDialog>
    );
  }
  return <BanDialog open={open} onClose={close} userId={row.user_id} who={who} channelId={channelId} />;
}

function StreamsTab({ channelId }: { channelId: string }) {
  const { t, locale } = useI18n();
  const { canApp } = useAuth();
  const toast = useToast();
  const streams = useChannelStreamsQuery(channelId);
  const del = useDeleteStreamMutation();
  const [deleting, setDeleting] = useState<T.LiveStream | null>(null);
  const now = useNow(10_000);
  const list = streams.data?.streams ?? [];

  const columns: Column<T.LiveStream>[] = [
    { key: "id", header: "ID", cell: (s) => <IdChip id={s.id} /> },
    { key: "mode", header: t("live.streams.mode"), cell: (s) => <Badge tone="neutral">{s.mode}</Badge> },
    { key: "format", header: t("live.streams.format"), cell: (s) => <span className="text-fg-muted">{s.format}</span> },
    { key: "state", header: t("common.status"), cell: (s) => <Badge tone={s.state === "streaming" ? "ok" : s.state === "reconnecting" ? "warn" : "neutral"}>{s.state}</Badge> },
    { key: "label", header: t("live.streams.label"), cell: (s) => <span>{s.label ?? "—"}</span> },
    { key: "frames", header: t("live.streams.frames"), align: "right", cell: (s) => <span className="tabular text-fg-muted">{fmtNumber(locale, s.frames_sent)}{s.frames_dropped ? <span className="text-warn"> · −{fmtNumber(locale, s.frames_dropped)}</span> : null}</span> },
    { key: "started", header: t("live.streams.started"), cell: (s) => <span className="text-fg-muted">{fmtRelative(locale, s.started_at, now)}</span> },
    {
      key: "actions",
      header: "",
      align: "right",
      width: "3rem",
      cell: (s) =>
        canApp("audio_streams:write") ? (
          <Menu items={[{ label: t("common.delete"), danger: true, onSelect: () => setDeleting(s) }]} />
        ) : null,
    },
  ];

  return (
    <Card>
      <CardHeader title={t("live.streams")} description={t("live.streams.hint")} />
      <DataTable
        rows={list}
        columns={columns}
        rowKey={(s) => s.id}
        loading={streams.isPending}
        error={streams.isError ? <QueryError error={streams.error} onRetry={() => void streams.refetch()} compact /> : undefined}
        empty={<EmptyState compact title={t("live.streams.empty")} description={t("live.streams.empty.desc")} />}
        dense
      />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => !o && setDeleting(null)}
        title={t("common.delete")}
        description={deleting ? t("live.streams.delete.confirm", { id: deleting.id }) : ""}
        confirmLabel={t("common.delete")}
        variant="danger"
        onConfirm={async () => {
          if (!deleting) return;
          try {
            await del.mutateAsync({ channelId, streamId: deleting.id });
            toast.ok(t("live.streams.deleted"));
          } catch (e) {
            throw new Error(errorMessage(e, locale), { cause: e });
          }
        }}
      />
    </Card>
  );
}
