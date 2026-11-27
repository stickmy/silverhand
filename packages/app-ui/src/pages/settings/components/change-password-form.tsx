import { useId, useState, type FormEvent } from "react";
import { ChevronDownIcon, ChevronRightIcon } from "@/icons";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  apiPaths,
  ChangePasswordInputSchema,
  ChangePasswordResponseSchema,
  PasswordStatusResponseSchema,
} from "@silverhand/shared";
import { api, ApiError } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { t } from "@/lib/i18n";
import { SettingsGroup, SettingsSection } from "./settings-section";

export function ChangePasswordForm({ userId }: { userId: string }): JSX.Element | null {
  const id = useId();
  const qc = useQueryClient();
  const status = useQuery({
    queryKey: ["password-status", userId],
    queryFn: async () => PasswordStatusResponseSchema.parse(await api(apiPaths.mePassword)),
  });
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [uncertain, setUncertain] = useState(false);
  // Collapsed by default: changing a password is a rare action, so it stays
  // out of the way until the viewer opens it.
  const [open, setOpen] = useState(false);

  if (!status.data?.has_password) return null;

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    if (pending || uncertain) return;
    const parsed = ChangePasswordInputSchema.safeParse({
      current_password: current,
      new_password: next,
    });
    if (!parsed.success) {
      setError(t("settings.password.invalid"));
      return;
    }
    if (next !== confirm) {
      setError(t("settings.password.mismatch"));
      return;
    }
    setPending(true);
    setError(null);
    try {
      // Deliberately no mutation cache or automatic retry for password plaintext.
      ChangePasswordResponseSchema.parse(
        await api(apiPaths.mePassword, { method: "POST", body: JSON.stringify(parsed.data) }),
      );
      setCurrent("");
      setNext("");
      setConfirm("");
      // Main clears only the session that issued this request. Re-read rather
      // than logging out again: a new login may have happened while awaiting it.
      await qc.invalidateQueries({ queryKey: ["me"] });
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 403) {
        setError(t("settings.password.rejected"));
      } else if (cause instanceof ApiError && cause.status === 422) {
        setError(t("settings.password.requirements"));
      } else {
        setUncertain(true);
        setCurrent("");
        setNext("");
        setConfirm("");
        setError(t("settings.password.uncertain"));
      }
    } finally {
      setPending(false);
    }
  }

  return (
    <SettingsSection label={t("settings.account.signIn")}>
      <SettingsGroup>
        <div data-settings-row-wrap>
          <button
            type="button"
            onClick={() => setOpen((prev) => !prev)}
            aria-expanded={open}
            className="flex min-h-12 w-full items-center justify-between gap-4 px-4 py-2.5 text-left text-sm font-semibold text-text-primary transition-colors hover:bg-bg-hover focus-outline"
          >
            <span>{t("settings.password.change")}</span>
            {open ? (
              <ChevronDownIcon className="size-4 shrink-0 text-text-muted" />
            ) : (
              <ChevronRightIcon className="size-4 shrink-0 text-text-muted" />
            )}
          </button>
          {open ? (
            <form
              onSubmit={(event) => void submit(event)}
              className="flex flex-col gap-3.5 border-t border-border-subtle bg-bg-base/55 px-4 py-3.5"
            >
              <div className="flex flex-col gap-1.5">
                <label
                  htmlFor={`${id}-current`}
                  className="text-sm font-semibold text-text-primary"
                >
                  {t("settings.password.current")}
                </label>
                <Input
                  id={`${id}-current`}
                  type="password"
                  autoComplete="current-password"
                  value={current}
                  onChange={(e) => setCurrent(e.target.value)}
                  disabled={pending || uncertain}
                  required
                  maxLength={1024}
                  className="h-8"
                />
              </div>
              <div className="flex flex-col gap-1.5">
                <label htmlFor={`${id}-new`} className="text-sm font-semibold text-text-primary">
                  {t("appShell.recovery.newPassword")}
                </label>
                <Input
                  id={`${id}-new`}
                  type="password"
                  autoComplete="new-password"
                  value={next}
                  onChange={(e) => setNext(e.target.value)}
                  disabled={pending || uncertain}
                  required
                  minLength={8}
                  maxLength={1024}
                  className="h-8"
                />
              </div>
              <div className="flex flex-col gap-1.5">
                <label
                  htmlFor={`${id}-confirm`}
                  className="text-sm font-semibold text-text-primary"
                >
                  {t("settings.password.confirmNew")}
                </label>
                <Input
                  id={`${id}-confirm`}
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  onChange={(e) => setConfirm(e.target.value)}
                  disabled={pending || uncertain}
                  required
                  maxLength={1024}
                  className="h-8"
                />
              </div>
              {error ? (
                <p role="alert" className="text-xs text-error">
                  {error}
                </p>
              ) : null}
              <div className="flex justify-end">
                <Button
                  type="submit"
                  variant="primary"
                  size="sm"
                  disabled={pending || uncertain || !current || !next || !confirm}
                >
                  {pending ? t("settings.saving") : t("settings.password.change")}
                </Button>
              </div>
            </form>
          ) : null}
        </div>
      </SettingsGroup>
    </SettingsSection>
  );
}
