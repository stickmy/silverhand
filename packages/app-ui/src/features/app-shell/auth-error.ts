import { AUTH_HANDLE_TAKEN_CODE } from "@silverhand/shared";
import { t } from "@/lib/i18n";

/** A submit failure that belongs to ONE field rather than to the form. Only the
 *  handle has such a case today (`handle_taken`): every other auth failure is
 *  either about the email+password PAIR or about the whole body, and pinning
 *  those to a single input would point the user at the wrong place. */
export interface AuthError {
  field: "handle" | null;
  message: string;
}

/**
 * Classify a submit failure into (field, message). The field is what decides
 * WHERE the message is rendered: a conflict on one unique field has to appear
 * under that field, while a credential failure describes the email+password pair
 * and belongs in the form footer.
 */
export function authErrorMessage(err: unknown): AuthError {
  const apiErr = err as { body?: string; message?: string };
  let parsed: { error?: string; error_code?: string; code?: string } = {};
  try {
    parsed = JSON.parse(apiErr.body ?? "{}");
  } catch {
    // not JSON — fall through.
  }
  if (parsed.error_code === AUTH_HANDLE_TAKEN_CODE || parsed.code === AUTH_HANDLE_TAKEN_CODE) {
    return { field: "handle", message: t("appShell.handleTaken") };
  }
  if (parsed.error_code === "auth_failed" || parsed.code === "auth_failed") {
    return { field: null, message: t("appShell.auth.incorrect") };
  }
  if (parsed.error === "invalid input") {
    return { field: null, message: t("appShell.checkFields") };
  }
  return {
    field: null,
    message: parsed.error ?? apiErr.message ?? t("appShell.auth.failed"),
  };
}
