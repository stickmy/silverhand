import { useEffect, useRef, useState, type FormEvent } from "react";
import {
  PasswordRecoveryRequestInputSchema,
  PasswordRecoveryResetInputSchema,
} from "@silverhand/shared";
import { SpinnerIcon } from "@/icons";
import { Input, Label } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { callPasswordRecovery, recoveryFailureMessage } from "./password-recovery";
import { t } from "@/lib/i18n";

type Step = "email" | "reset";

/**
 * Email recovery-code password reset against the server selected when the flow
 * opened. A successful reset returns to the login page without signing in.
 */
export function PasswordRecoveryForm({
  enabled,
  initialEmail,
  onDone,
  onCancel,
}: {
  enabled: boolean;
  initialEmail: string;
  onDone: (email: string) => void;
  onCancel: () => void;
}): JSX.Element {
  const [step, setStep] = useState<Step>("email");
  const [email, setEmail] = useState(initialEmail);
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Answers that arrive after the form closed belong to no flow.
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const emailTrimmed = email.trim();

  async function requestCode(event?: FormEvent) {
    event?.preventDefault();
    const input = PasswordRecoveryRequestInputSchema.safeParse({ email: emailTrimmed });
    if (!input.success || pending) return;
    setPending(true);
    setError(null);
    try {
      const result = await callPasswordRecovery("request", input.data);
      if (!alive.current) return;
      if (result.status === 202) {
        setStep("reset");
        setNotice(t("appShell.recovery.codeSent"));
      } else {
        setError(recoveryFailureMessage(result.status, result.code));
      }
    } catch {
      if (alive.current) setError(t("common.unreachable"));
    } finally {
      if (alive.current) setPending(false);
    }
  }

  async function reset(event: FormEvent) {
    event.preventDefault();
    if (pending) return;
    if (password !== confirm) {
      setError(t("appShell.passwordMismatch"));
      return;
    }
    const input = PasswordRecoveryResetInputSchema.safeParse({
      email: emailTrimmed,
      code: code.trim(),
      new_password: password,
    });
    if (!input.success) {
      setError(t("appShell.recovery.enterCode"));
      return;
    }
    setPending(true);
    setError(null);
    try {
      const result = await callPasswordRecovery("reset", input.data);
      if (!alive.current) return;
      if (result.status === 200) onDone(emailTrimmed);
      else setError(recoveryFailureMessage(result.status, result.code));
    } catch {
      if (alive.current) setError(t("common.unreachable"));
    } finally {
      if (alive.current) setPending(false);
    }
  }

  const back = (
    <button
      type="button"
      className="font-medium text-text-primary underline underline-offset-3 hover:text-text-secondary focus-outline"
      onClick={onCancel}
    >
      {t("appShell.recovery.backToLogin")}
    </button>
  );

  if (!enabled) {
    return (
      <div className="flex flex-col gap-4">
        <p className="text-sm text-text-secondary">{t("appShell.recovery.disabledReset")}</p>
        <p className="text-sm text-text-muted">{back}</p>
      </div>
    );
  }

  return step === "email" ? (
    <form onSubmit={(event) => void requestCode(event)} className="flex flex-col gap-4">
      <p className="text-sm text-text-muted">{t("appShell.recovery.intro")}</p>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="recovery-email">{t("appShell.email")}</Label>
        <Input
          id="recovery-email"
          type="email"
          value={email}
          onChange={(event) => setEmail(event.target.value)}
          autoComplete="email"
          spellCheck={false}
          placeholder="you@example.com"
          autoFocus
        />
      </div>
      <Button
        type="submit"
        variant="primary"
        size="lg"
        className="w-full justify-center"
        disabled={pending || emailTrimmed.length === 0}
      >
        {pending ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
        {t("appShell.recovery.sendCode")}
      </Button>
      {error ? <p className="text-xs text-error">{error}</p> : null}
      <p className="text-sm text-text-muted">{back}</p>
    </form>
  ) : (
    <form onSubmit={(event) => void reset(event)} className="flex flex-col gap-4">
      {notice ? <p className="text-sm text-text-muted">{notice}</p> : null}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="recovery-code">{t("appShell.recovery.code")}</Label>
        <Input
          id="recovery-code"
          value={code}
          onChange={(event) => setCode(event.target.value.replace(/\D/g, "").slice(0, 6))}
          inputMode="numeric"
          autoComplete="one-time-code"
          placeholder={t("appShell.recovery.codePlaceholder")}
          autoFocus
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="recovery-password">{t("appShell.recovery.newPassword")}</Label>
        <Input
          id="recovery-password"
          type="password"
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          autoComplete="new-password"
          placeholder={t("appShell.passwordMin")}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="recovery-confirm">{t("appShell.confirmPassword")}</Label>
        <Input
          id="recovery-confirm"
          type="password"
          value={confirm}
          onChange={(event) => setConfirm(event.target.value)}
          autoComplete="new-password"
          placeholder={t("appShell.repeatPassword")}
        />
      </div>
      <Button
        type="submit"
        variant="primary"
        size="lg"
        className="w-full justify-center"
        disabled={pending || code.length !== 6 || password.length < 8 || confirm.length === 0}
      >
        {pending ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
        {t("appShell.resetPassword")}
      </Button>
      {error ? <p className="text-xs text-error">{error}</p> : null}
      <p className="text-sm text-text-muted">
        <button
          type="button"
          className="font-medium text-text-primary underline underline-offset-3 hover:text-text-secondary focus-outline disabled:opacity-50"
          disabled={pending}
          onClick={() => void requestCode()}
        >
          {t("appShell.recovery.sendNewCode")}
        </button>
        <span className="mx-2">·</span>
        {back}
      </p>
    </form>
  );
}
