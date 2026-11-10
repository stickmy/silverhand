import { getPlatform } from "@/platform";
import { t } from "@/lib/i18n";

/** One recovery-code call against the server selected when the flow opened. */
export async function callPasswordRecovery(action: "request" | "reset", body: unknown) {
  const result = await getPlatform().client.passwordRecovery({
    action,
    body: JSON.stringify(body),
  });
  let parsed: { error_code?: string } = {};
  try {
    parsed = JSON.parse(result.bodyText);
  } catch {
    // not JSON
  }
  return { status: result.status, code: parsed.error_code };
}

export function recoveryFailureMessage(status: number, code: string | undefined): string {
  switch (code) {
    case "invalid_or_expired_code":
      return t("appShell.recovery.codeInvalid");
    case "password_recovery_disabled":
      return t("appShell.recovery.disabled");
    case "password_recovery_busy":
    case "rate_limited":
      return t("appShell.recovery.tooMany");
  }
  if (status === 409) return t("appShell.recovery.serverChanged");
  if (status === 0) return t("common.unreachable");
  if (status === 422) return t("appShell.checkFields");
  return t("common.somethingWrong");
}
