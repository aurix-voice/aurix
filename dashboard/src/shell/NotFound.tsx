import { Link } from "@tanstack/react-router";
import { Compass } from "lucide-react";

import { useI18n } from "@/i18n";
import { Button } from "@/ui/Button";
import { EmptyState } from "@/ui/Primitives";

export function NotFound() {
  const { t } = useI18n();
  return (
    <div className="min-h-dvh flex items-center justify-center p-6">
      <EmptyState
        icon={<Compass className="size-6" />}
        title={t("common.notFound")}
        description={t("common.notFound.desc")}
        action={
          <Link to="/">
            <Button variant="secondary">{t("nav.overview")}</Button>
          </Link>
        }
      />
    </div>
  );
}
