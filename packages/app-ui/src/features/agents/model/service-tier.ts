import { t } from "@/lib/i18n";

/** Known Codex tier ids get localized names; others use the CLI's name or the raw id. */
export function serviceTierLabel(id: string, cliName?: string): string {
  if (id === "default") return t("agents.serviceTierStandard");
  if (id === "priority") return t("agents.serviceTierFast");
  if (id === "ultrafast") return t("agents.serviceTierUltrafast");
  return cliName ?? id;
}
