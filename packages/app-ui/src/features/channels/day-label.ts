import dayjs from "dayjs";
import localizedFormat from "dayjs/plugin/localizedFormat";
import { t } from "@/lib/i18n";

dayjs.extend(localizedFormat);

/** Today / Yesterday, else the active locale's medium date (`Nov 10, 2026`). */
export function dayLabel(msSinceEpoch: number, nowMs: number): string {
  const day = dayjs(msSinceEpoch);
  const now = dayjs(nowMs);
  if (day.isSame(now, "day")) return t("channels.today");
  if (day.isSame(now.subtract(1, "day"), "day")) return t("channels.yesterday");
  return day.format("ll");
}
