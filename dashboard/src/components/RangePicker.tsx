import { useMemo, useState } from "react";

import { useI18n } from "@/i18n";
import { isoDay, toIsoEnd, toIsoStart } from "@/lib/format";
import { Input } from "@/ui/Input";
import { Segmented } from "@/ui/Menu";

export type RangePreset = "24h" | "7d" | "30d" | "custom";

export interface Range {
  from: string;
  to: string;
  preset: RangePreset;
}

const PRESET_MS: Record<Exclude<RangePreset, "custom">, number> = {
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "30d": 30 * 86_400_000,
};

export function presetRange(preset: Exclude<RangePreset, "custom">, now = Date.now()): Range {
  return { from: new Date(now - PRESET_MS[preset]).toISOString(), to: new Date(now).toISOString(), preset };
}

/** Preset + custom day range. `to` is exclusive, matching the API. */
export function useRange(initial: Exclude<RangePreset, "custom"> = "7d"): [Range, (r: Range) => void] {
  const [range, setRange] = useState<Range>(() => presetRange(initial));
  return [range, setRange];
}

/** Step (seconds) keeping a series under ~400 points. */
export function stepFor(range: Range): number {
  const span = (Date.parse(range.to) - Date.parse(range.from)) / 1000;
  const candidates = [300, 900, 3600, 6 * 3600, 86_400];
  return candidates.find((s) => span / s <= 400) ?? 86_400;
}

export function RangePicker({ value, onChange }: { value: Range; onChange: (r: Range) => void }) {
  const { t } = useI18n();
  const days = useMemo(
    () => ({ from: isoDay(new Date(value.from)), to: isoDay(new Date(Date.parse(value.to) - 1)) }),
    [value.from, value.to],
  );
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Segmented
        size="sm"
        value={value.preset}
        onChange={(p) => {
          if (p === "custom") onChange({ ...value, preset: "custom" });
          else onChange(presetRange(p));
        }}
        options={[
          { value: "24h", label: t("common.last24h") },
          { value: "7d", label: t("common.last7d") },
          { value: "30d", label: t("common.last30d") },
          { value: "custom", label: t("common.custom") },
        ]}
      />
      {value.preset === "custom" ? (
        <div className="flex items-center gap-1.5">
          <Input
            type="date"
            className="h-8 w-[9.5rem]"
            value={days.from}
            max={days.to}
            onChange={(e) => e.target.value && onChange({ ...value, from: toIsoStart(e.target.value), preset: "custom" })}
            aria-label={t("common.from")}
          />
          <span className="text-fg-faint">–</span>
          <Input
            type="date"
            className="h-8 w-[9.5rem]"
            value={days.to}
            min={days.from}
            onChange={(e) => e.target.value && onChange({ ...value, to: toIsoEnd(e.target.value), preset: "custom" })}
            aria-label={t("common.to")}
          />
        </div>
      ) : null}
    </div>
  );
}
