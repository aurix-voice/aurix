import { useState } from "react";

import { errorMessage, type T } from "@/api/client";
import { useBanUserMutation, useKickMutation } from "@/api/hooks";
import { useI18n } from "@/i18n";
import { ConfirmDialog } from "@/ui/Dialog";
import { Checkbox, Input, NativeSelect, Textarea } from "@/ui/Input";
import { Field } from "@/ui/Primitives";
import { useToast } from "@/ui/Toast";

import { isUuid, UserPicker } from "./UserPicker";

/**
 * Ban confirmation shared by the Live channel page and Moderation. With `channelId` the user can
 * also be kicked from that channel in the same step; without `userId` the target is picked here.
 */
export function BanDialog({
  open,
  onClose,
  userId,
  who,
  channelId,
}: {
  open: boolean;
  onClose: () => void;
  userId?: string;
  who?: string;
  channelId?: string;
}) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const ban = useBanUserMutation();
  const kick = useKickMutation();
  const [target, setTarget] = useState("");
  const [reason, setReason] = useState("");
  const [scope, setScope] = useState<T.BanScope>("account");
  const [deviceId, setDeviceId] = useState("");
  const [ip, setIp] = useState("");
  const [hours, setHours] = useState("");
  const [alsoKick, setAlsoKick] = useState(true);

  const close = () => {
    setTarget("");
    setReason("");
    setScope("account");
    setDeviceId("");
    setIp("");
    setHours("");
    setAlsoKick(true);
    onClose();
  };

  const user = userId ?? target.trim();
  const h = hours.trim() === "" ? null : Number(hours);
  const hoursOk = h === null || (Number.isFinite(h) && h > 0);
  const scopeOk = scope === "account" || (scope === "device" ? deviceId.trim() !== "" : ip.trim() !== "");
  const ready = isUuid(user) && reason.trim() !== "" && hoursOk && scopeOk;

  return (
    <ConfirmDialog
      open={open}
      onOpenChange={(o) => !o && close()}
      title={t("live.ban")}
      description={who ? t("live.ban.confirm", { who }) : t("moderation.ban.new.desc")}
      confirmLabel={t("live.ban")}
      variant="danger"
      disabled={!ready}
      onConfirm={async () => {
        try {
          await ban.mutateAsync({
            user_id: user,
            scope,
            reason: reason.trim(),
            duration_hours: h,
            device_id: scope === "device" ? deviceId.trim() : null,
            ip_address: scope === "ip_address" ? ip.trim() : null,
          });
          if (channelId && alsoKick) await kick.mutateAsync({ user_id: user, channel_id: channelId, reason: reason.trim() });
          toast.ok(t("live.banned.done"));
        } catch (e) {
          throw new Error(errorMessage(e, locale), { cause: e });
        }
      }}
    >
      {userId ? null : (
        <Field label={t("common.user")} required>
          <UserPicker value={target} onChange={setTarget} autoFocus />
        </Field>
      )}
      <Field label={t("live.reason")} required>
        <Textarea value={reason} onChange={(e) => setReason(e.target.value)} rows={2} maxLength={512} autoFocus={!!userId} />
      </Field>
      <div className="grid grid-cols-2 gap-3">
        <Field label={t("live.ban.scope")}>
          <NativeSelect value={scope} onChange={(e) => setScope(e.target.value as T.BanScope)}>
            <option value="account">{t("live.ban.scope.account")}</option>
            <option value="device">{t("live.ban.scope.device")}</option>
            <option value="ip_address">{t("live.ban.scope.ip")}</option>
          </NativeSelect>
        </Field>
        <Field label={t("live.ban.hours")} hint={t("live.ban.hours.hint")} error={hoursOk ? undefined : t("common.invalidNumber")}>
          <Input value={hours} onChange={(e) => setHours(e.target.value)} inputMode="numeric" placeholder="∞" />
        </Field>
      </div>
      {scope === "device" ? (
        <Field label={t("moderation.ban.deviceId")} required>
          <Input value={deviceId} onChange={(e) => setDeviceId(e.target.value)} className="font-mono text-[12.5px]" spellCheck={false} />
        </Field>
      ) : null}
      {scope === "ip_address" ? (
        <Field label={t("moderation.ban.ip")} required>
          <Input value={ip} onChange={(e) => setIp(e.target.value)} className="font-mono text-[12.5px]" spellCheck={false} placeholder="203.0.113.7" />
        </Field>
      ) : null}
      {channelId ? <Checkbox checked={alsoKick} onChange={(e) => setAlsoKick(e.target.checked)} label={t("live.ban.alsoKick")} /> : null}
    </ConfirmDialog>
  );
}
