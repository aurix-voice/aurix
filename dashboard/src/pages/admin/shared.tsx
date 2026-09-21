import type { T } from "@/api/client";
import { useI18n } from "@/i18n";
import { NativeSelect } from "@/ui/Input";
import { Badge, Field, type Tone } from "@/ui/Primitives";

import { ADMIN_ROLES, roleAdds } from "./model";

const ROLE_TONE: Record<T.AdminRole, Tone> = { viewer: "neutral", moderator: "accent", admin: "ok", superadmin: "warn" };

export function RoleBadge({ role }: { role: T.AdminRole }) {
  const { t } = useI18n();
  return <Badge tone={ROLE_TONE[role]}>{t(`role.${role}`)}</Badge>;
}

export function SourceBadge({ admin }: { admin: T.Admin }) {
  const { t } = useI18n();
  return (
    <span className="inline-flex items-center gap-1">
      <Badge tone="neutral">{t(`admin.source.${admin.auth_source}`)}</Badge>
      {admin.sso_bound && admin.auth_source !== "oidc" ? <Badge tone="neutral">{t("admin.ssoBound")}</Badge> : null}
      {admin.has_password === false ? <Badge tone="neutral">{t("admin.ssoOnly")}</Badge> : null}
    </span>
  );
}

/** Role picker with the incremental permission list of the chosen role. */
export function RoleField({ value, onChange, disabled, id = "admin-role" }: { value: T.AdminRole; onChange: (r: T.AdminRole) => void; disabled?: boolean; id?: string }) {
  const { t } = useI18n();
  return (
    <Field label={t("admin.role")} htmlFor={id} hint={t("admin.role.desc")}>
      <NativeSelect id={id} value={value} onChange={(e) => onChange(e.target.value as T.AdminRole)} disabled={disabled}>
        {ADMIN_ROLES.map((r) => (
          <option key={r} value={r}>
            {t(`role.${r}`)}
          </option>
        ))}
      </NativeSelect>
      <ul className="mt-1 flex flex-wrap gap-1">
        {ADMIN_ROLES.filter((r) => ADMIN_ROLES.indexOf(r) <= ADMIN_ROLES.indexOf(value)).flatMap((r) =>
          roleAdds(r).map((p) => (
            <li key={p}>
              <Badge tone={r === value ? "accent" : "neutral"}>{t(`perm.${p}`)}</Badge>
            </li>
          )),
        )}
      </ul>
    </Field>
  );
}
