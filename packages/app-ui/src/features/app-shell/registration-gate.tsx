import { ServerAddressControl, ServerConnectionStatus } from "./server-address-control";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { SpinnerIcon } from "@/icons";
import { suggestParticipantNameFromEmail } from "@silverhand/shared";
import { authErrorMessage, type AuthError } from "./auth-error";
import { Input, Label } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { nameFormatMessage } from "@/features/identity/use-name-availability";
import { cn } from "@/lib/cn";
import { AuthSplitLayout } from "./auth-split-layout";
import { ExternalLink, PRIVACY_POLICY_URL } from "@/features/legal/legal-links";
import { DraftAvatarCard } from "@/features/avatar-maker/components/draft-avatar-card";
import { useDraftAvatar } from "@/features/avatar-maker/hooks/use-draft-avatar";
import { useLoginUser, useSignupUser, useUploadDraftViewerAvatar } from "../session/hooks";
import { PasswordRecoveryForm } from "./password-recovery-form";
import { t } from "@/lib/i18n";
import { getPlatform } from "@/platform";
import { NativeAuthScreen } from "./native-auth-screen";

export interface RegistrationGateProps {
  signupEnabled: boolean;
  /** Absent hides the recovery entry (the embedded local server). */
  passwordRecovery?: "enabled" | "disabled";
  preferLogin?: boolean;
  autoFocus?: boolean;
  connectionReady?: boolean;
  connectionStatus?: string;
  connectionChecking?: boolean;
  showServerAddress?: boolean;
  onRetry?: () => void;
}

type AuthMode = "signup" | "login";

/**
 * Full-screen desktop auth gate. The renderer never receives the
 * `user_token`: submit calls an IPC method, main performs `/auth/*`,
 * persists the main-owned 0600 local session file, and returns only
 * the resolved user.
 *
 * Signup asks for the handle explicitly (#task:1830). It is the one field here
 * that can never be corrected afterwards, which is why it carries a note, is
 * required, sits directly under the email it is prefilled from, and stops
 * following that email the moment the user edits it.
 */
export function RegistrationGate(props: RegistrationGateProps): JSX.Element {
  // iOS draws the whole signed-out screen natively at every width, so a Duo
  // unfolding mid-form keeps the same screen and what was typed.
  const screen = getPlatform().authScreen;

  return screen ? <NativeAuthScreen screen={screen} {...props} /> : <RegistrationForm {...props} />;
}

function RegistrationForm({
  signupEnabled,
  passwordRecovery,
  preferLogin = false,
  autoFocus = true,
  connectionReady = true,
  connectionStatus,
  connectionChecking,
  showServerAddress = false,
  onRetry,
}: RegistrationGateProps): JSX.Element {
  const signup = useSignupUser();
  const login = useLoginUser();
  const uploadDraftAvatar = useUploadDraftViewerAvatar();
  const draftAvatar = useDraftAvatar();
  const [mode, setMode] = useState<AuthMode>(signupEnabled && !preferLogin ? "signup" : "login");
  const [email, setEmail] = useState("");
  const [handle, setHandle] = useState("");
  const [handleEdited, setHandleEdited] = useState(false);
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [error, setError] = useState<AuthError | null>(null);
  const [editingServer, setEditingServer] = useState(false);
  const [recovering, setRecovering] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const emailInputRef = useRef<HTMLInputElement | null>(null);
  const handleInputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (!signupEnabled) setMode("login");
  }, [signupEnabled]);

  useEffect(() => {
    if (autoFocus) emailInputRef.current?.focus();
  }, [autoFocus]);

  const submitting = signup.isPending || login.isPending;
  const emailTrimmed = email.trim();
  const handleTrimmed = handle.trim();
  const displayNameTrimmed = displayName.trim();
  const isSignup = mode === "signup";
  // Shared format seam — never a second regex or a second wording (#task:1827).
  // `agent` is the naming-era label for the participant/mention grammar, which
  // is exactly what a handle is, so the words match create-agent and rename.
  const handleFormatMessage = nameFormatMessage("agent", handleTrimmed);
  const canSubmit =
    connectionReady &&
    !editingServer &&
    !submitting &&
    emailTrimmed.length > 0 &&
    password.length >= (isSignup ? 8 : 1) &&
    (!isSignup ||
      (handleTrimmed.length > 0 && handleFormatMessage === null && password === confirmPassword));

  /** Prefill the handle from the email until the user takes the field over. A
   *  dirty flag rather than "empty means follow": typing a handle and then going
   *  back to finish the email must not overwrite the deliberate choice. */
  const onEmailChange = (next: string) => {
    setEmail(next);
    if (!handleEdited) setHandle(suggestParticipantNameFromEmail(next.trim()));
  };

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    setError(null);
    setNotice(null);
    try {
      if (isSignup) {
        await signup.mutateAsync({
          email: emailTrimmed,
          password,
          name: handleTrimmed,
          display_name: displayNameTrimmed.length > 0 ? displayNameTrimmed : undefined,
        });
        uploadDraftAvatar(draftAvatar);
      } else {
        await login.mutateAsync({ email: emailTrimmed, password });
      }
    } catch (err) {
      const authError = authErrorMessage(err);
      setError(authError);
      // There is no availability probe by design, so a taken handle can only
      // come back after submit. Put the cursor where the fix has to happen and
      // preselect the value — everything else the user typed stays untouched.
      if (authError.field === "handle") {
        handleInputRef.current?.focus();
        handleInputRef.current?.select();
      }
    }
  };

  const formError = error?.field === null ? error.message : null;
  const handleError = handleFormatMessage ?? (error?.field === "handle" ? error.message : null);

  if (recovering && passwordRecovery) {
    return (
      <AuthSplitLayout>
        <div
          className="w-full max-w-90"
          style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
        >
          <header className="mb-7 flex flex-col gap-1.5">
            <h1 className="text-3xl font-semibold tracking-tight">{t("appShell.resetPassword")}</h1>
          </header>
          <PasswordRecoveryForm
            enabled={passwordRecovery === "enabled"}
            initialEmail={emailTrimmed}
            onCancel={() => setRecovering(false)}
            onDone={(recoveredEmail) => {
              setEmail(recoveredEmail);
              setPassword("");
              setError(null);
              setNotice(t("appShell.auth.passwordReset"));
              setRecovering(false);
            }}
          />
        </div>
      </AuthSplitLayout>
    );
  }

  return (
    <AuthSplitLayout>
      <form
        onSubmit={onSubmit}
        // Sits on the open page, not in a card: the brand column already frames
        // the screen, so a second container only adds weight.
        data-auth-form
        className="w-full max-w-90"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        <header className="mb-7 flex flex-col gap-1.5">
          <h1 className="text-3xl font-semibold tracking-tight">
            {isSignup ? t("appShell.auth.signUp") : t("appShell.auth.logIn")}
          </h1>
          <p className="text-sm text-text-muted">
            {isSignup ? t("appShell.auth.createAccount") : t("appShell.auth.welcomeBack")}
          </p>
        </header>

        <div data-auth-fields className="flex flex-col gap-4">
          {isSignup ? <DraftAvatarCard draft={draftAvatar} disabled={submitting} /> : null}

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="auth-email">{t("appShell.email")}</Label>
            <Input
              ref={emailInputRef}
              id="auth-email"
              type="email"
              value={email}
              onChange={(e) => onEmailChange(e.target.value)}
              autoComplete="email"
              spellCheck={false}
              placeholder="you@example.com"
            />
          </div>

          {isSignup ? (
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="auth-handle" required>
                {t("appShell.handle")}
              </Label>
              <Input
                ref={handleInputRef}
                id="auth-handle"
                value={handle}
                onChange={(e) => {
                  setHandleEdited(true);
                  setHandle(e.target.value);
                  // A field-scoped error describes the value that was submitted.
                  // Since a `handle_taken` failure focuses AND selects this
                  // input, the next keystroke replaces exactly that value — and
                  // a stale "already taken" under a freshly typed handle reads
                  // as "this one is taken too".
                  if (error?.field === "handle") setError(null);
                }}
                autoComplete="username"
                spellCheck={false}
                placeholder={t("appShell.auth.handlePlaceholder")}
              />
              {/* One line, always occupied: the problem when there is one, the
                  irreversibility note otherwise. Same height either way, so
                  nothing moves — and no reserved empty row. */}
              <p
                className={cn("text-xs leading-4", handleError ? "text-error" : "text-text-muted")}
              >
                {handleError ?? t("appShell.auth.handlePermanent")}
              </p>
            </div>
          ) : null}

          {isSignup ? (
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="auth-display">{t("appShell.displayName")}</Label>
              <Input
                id="auth-display"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                autoComplete="name"
                placeholder={t("appShell.auth.optional")}
              />
            </div>
          ) : null}

          <div className="flex flex-col gap-1.5">
            <div className="flex items-baseline justify-between gap-3">
              <Label htmlFor="auth-password">{t("appShell.password")}</Label>
              {!isSignup && passwordRecovery && connectionReady ? (
                <button
                  type="button"
                  className="text-xs text-text-muted underline underline-offset-3 hover:text-text-secondary focus-outline"
                  disabled={submitting}
                  onClick={() => {
                    setError(null);
                    setNotice(null);
                    setRecovering(true);
                  }}
                >
                  {t("appShell.auth.forgotPassword")}
                </button>
              ) : null}
            </div>
            <Input
              id="auth-password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete={isSignup ? "new-password" : "current-password"}
              placeholder={isSignup ? t("appShell.passwordMin") : t("appShell.password")}
            />
          </div>

          {isSignup ? (
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="auth-confirm">{t("appShell.confirmPassword")}</Label>
              <Input
                id="auth-confirm"
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                autoComplete="new-password"
                placeholder={t("appShell.repeatPassword")}
              />
            </div>
          ) : null}
        </div>

        <footer className="mt-7 flex flex-col gap-3">
          <Button
            type="submit"
            variant="primary"
            size="lg"
            className="w-full justify-center"
            disabled={!canSubmit}
          >
            {submitting ? (
              <>
                <SpinnerIcon className="size-3.5 animate-spin" />
                {isSignup ? t("appShell.auth.creating") : t("appShell.auth.signingIn")}
              </>
            ) : isSignup ? (
              t("appShell.auth.createAccountButton")
            ) : (
              t("appShell.auth.logIn")
            )}
          </Button>
          {isSignup && password.length > 0 && password !== confirmPassword ? (
            // `text-error` — the theme token. `text-status-error`, which these
            // two lines used, is not defined in any theme, so both rendered in
            // the default text colour.
            <p className="text-xs text-error">{t("appShell.passwordMismatch")}</p>
          ) : null}
          {formError ? <p className="text-xs text-error">{formError}</p> : null}
          {notice && !formError ? <p className="text-xs text-text-muted">{notice}</p> : null}
          {signupEnabled ? (
            <p className="text-sm text-text-muted">
              {isSignup ? t("appShell.auth.haveAccount") : t("appShell.auth.noAccount")}{" "}
              <button
                type="button"
                className="font-medium text-text-primary underline underline-offset-3 hover:text-text-secondary focus-outline"
                onClick={() => {
                  setMode(isSignup ? "login" : "signup");
                  setError(null);
                  setNotice(null);
                }}
              >
                {isSignup ? t("appShell.auth.logIn") : t("appShell.auth.signUp")}
              </button>
            </p>
          ) : null}
        </footer>

        <div
          data-auth-legal
          className="mt-7 flex flex-wrap items-center justify-between gap-x-4 gap-y-2 text-[13px] text-text-muted"
        >
          <ExternalLink href={PRIVACY_POLICY_URL} className="hover:text-text-secondary">
            {t("appShell.privacyPolicy")}
          </ExternalLink>
          {showServerAddress ? (
            <ServerAddressControl
              onRetry={onRetry}
              onEditingChange={setEditingServer}
              disabled={submitting}
            />
          ) : null}
        </div>

        {showServerAddress ? (
          <ServerConnectionStatus
            status={connectionStatus}
            checking={connectionChecking}
            onRetry={onRetry}
          />
        ) : null}
      </form>
    </AuthSplitLayout>
  );
}
