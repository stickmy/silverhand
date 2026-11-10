import { useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { AuthScreenCapability, AuthScreenEvent, AuthScreenSheet } from "@silverhand/client";
import {
  PasswordRecoveryRequestInputSchema,
  PasswordRecoveryResetInputSchema,
  suggestParticipantNameFromEmail,
} from "@silverhand/shared";
import { randomAvatarConfig } from "@/features/avatar-maker/model/avatar-config";
import { useDraftAvatar } from "@/features/avatar-maker/hooks/use-draft-avatar";
import { useAppearance } from "@/features/appearance/use-appearance";
import { nameFormatMessage } from "@/features/identity/use-name-availability";
import { PRIVACY_POLICY_URL } from "@/features/legal/legal-links";
import { useServerEnvironment } from "@/features/session/server-environment";
import { t, useLocale } from "@/lib/i18n";
import { useLoginUser, useSignupUser, useUploadDraftViewerAvatar } from "../session/hooks";
import { authErrorMessage, type AuthError } from "./auth-error";
import { callPasswordRecovery, recoveryFailureMessage } from "./password-recovery";
import { describeConnectionFailure, saveServerAddress } from "./server-address-control";
import type { RegistrationGateProps } from "./registration-gate";

type Sheet = "login" | "signup" | "recovery" | "server";

/** Fixed faces for the scene, so it reads the same on every launch. */
function sceneFace(seed: number) {
  let state = seed;

  return randomAvatarConfig(undefined, () => {
    state = (state * 16807) % 2147483647;
    return state / 2147483647;
  });
}

/**
 * The phone's signed-out screen, drawn natively: the product speaks first, and
 * the forms wait in sheets behind Log in and Create account. This keeps the
 * session calls, validation and every string; native keeps the fields.
 */
export function NativeAuthScreen({
  screen,
  signupEnabled,
  passwordRecovery,
  connectionReady = true,
  connectionStatus,
  connectionChecking,
  showServerAddress = false,
  onRetry,
}: RegistrationGateProps & { screen: AuthScreenCapability }): JSX.Element {
  const qc = useQueryClient();
  const signup = useSignupUser();
  const login = useLoginUser();
  const uploadDraftAvatar = useUploadDraftViewerAvatar();
  const draftAvatar = useDraftAvatar();
  const environment = useServerEnvironment();
  const { appearance } = useAppearance();
  const locale = useLocale();
  const [sheet, setSheet] = useState<Sheet | null>(null);
  const [email, setEmail] = useState("");
  const [handle, setHandle] = useState("");
  const [handleEdited, setHandleEdited] = useState(false);
  const [error, setError] = useState<AuthError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [recoveryStep, setRecoveryStep] = useState<"email" | "reset">("email");
  const [recoveryEmail, setRecoveryEmail] = useState("");
  const [busy, setBusy] = useState(false);
  const [sheetError, setSheetError] = useState<string | null>(null);
  const faces = useMemo(
    () => ({ leo: sceneFace(11), doge: sceneFace(29), typist: sceneFace(53) }),
    [],
  );

  const submitting = signup.isPending || login.isPending;
  const handleTrimmed = handle.trim();
  // Shared format seam with the full form — never a second regex or wording.
  const handleFormatMessage = handleTrimmed ? nameFormatMessage("agent", handleTrimmed) : null;
  const handleError = handleFormatMessage ?? (error?.field === "handle" ? error.message : null);
  const serverUrl = environment.data?.url ?? "";

  const open = (next: Sheet | null) => {
    setError(null);
    setNotice(null);
    setSheetError(null);
    setSheet(next);
  };

  const submit = async (input: { email: string; password: string; handle?: string }) => {
    if (!connectionReady || submitting) return;
    setError(null);
    setNotice(null);
    try {
      if (sheet === "signup") {
        await signup.mutateAsync({
          email: input.email,
          password: input.password,
          name: input.handle ?? "",
        });
        uploadDraftAvatar(draftAvatar);
      } else {
        await login.mutateAsync({ email: input.email, password: input.password });
      }
    } catch (err) {
      setError(authErrorMessage(err));
    }
  };

  const requestCode = async (address: string) => {
    const input = PasswordRecoveryRequestInputSchema.safeParse({ email: address.trim() });
    if (!input.success || busy) return;
    setBusy(true);
    setSheetError(null);
    try {
      const result = await callPasswordRecovery("request", input.data);
      if (result.status === 202) {
        setRecoveryEmail(input.data.email);
        setRecoveryStep("reset");
      } else {
        setSheetError(recoveryFailureMessage(result.status, result.code));
      }
    } catch {
      setSheetError(t("common.unreachable"));
    } finally {
      setBusy(false);
    }
  };

  const resetPassword = async (code: string, password: string, confirm: string) => {
    if (busy) return;
    if (password !== confirm) {
      setSheetError(t("appShell.passwordMismatch"));
      return;
    }
    const input = PasswordRecoveryResetInputSchema.safeParse({
      email: recoveryEmail,
      code: code.trim(),
      new_password: password,
    });
    if (!input.success) {
      setSheetError(t("appShell.recovery.enterCode"));
      return;
    }
    setBusy(true);
    setSheetError(null);
    try {
      const result = await callPasswordRecovery("reset", input.data);
      if (result.status === 200) {
        setEmail(recoveryEmail);
        open("login");
        setNotice(t("appShell.auth.passwordReset"));
      } else {
        setSheetError(recoveryFailureMessage(result.status, result.code));
      }
    } catch {
      setSheetError(t("common.unreachable"));
    } finally {
      setBusy(false);
    }
  };

  const saveServer = async (url: string) => {
    if (busy) return;
    setBusy(true);
    setSheetError(null);
    const failure = await saveServerAddress(qc, url);
    setBusy(false);
    if (failure) {
      setSheetError(failure);
      return;
    }
    open(null);
    onRetry?.();
  };

  const onEvent = (event: AuthScreenEvent) => {
    switch (event.kind) {
      case "open":
        open(event.sheet);
        break;
      case "dismiss":
        open(null);
        break;
      case "input":
        if (event.field === "email") {
          setEmail(event.value);
          if (!handleEdited) setHandle(suggestParticipantNameFromEmail(event.value.trim()));
        } else {
          setHandleEdited(true);
          setHandle(event.value);
          if (error?.field === "handle") setError(null);
        }
        break;
      case "avatar":
        draftAvatar.compose(event.config);
        break;
      case "submit":
        void submit({
          email: event.email.trim(),
          password: event.password,
          handle: event.handle?.trim(),
        });
        break;
      case "forgot":
        setEmail(event.email);
        setRecoveryEmail(event.email.trim());
        setRecoveryStep("email");
        open("recovery");
        break;
      case "recoveryRequest":
        void requestCode(event.email);
        break;
      case "recoveryReset":
        void resetPassword(event.code, event.password, event.confirm);
        break;
      case "recoveryBack":
        open("login");
        break;
      case "serverSave":
        void saveServer(event.url);
        break;
      case "retry":
        onRetry?.();
        break;
    }
  };
  const events = useRef(onEvent);
  events.current = onEvent;

  useEffect(() => {
    let unsubscribe: (() => void) | undefined;
    let alive = true;
    void screen
      .subscribe((event) => events.current(event))
      .then((stop) => {
        if (alive) {
          unsubscribe = stop;
        } else {
          stop();
        }
      });

    return () => {
      alive = false;
      unsubscribe?.();
      void screen.hide();
    };
  }, [screen]);

  const sheetState = (): AuthScreenSheet | undefined => {
    const close = t("common.close");
    switch (sheet) {
      case null:
        return undefined;
      case "login":
      case "signup": {
        const signingUp = sheet === "signup";
        const label = signingUp ? t("appShell.auth.createAccountButton") : t("appShell.auth.logIn");
        return {
          kind: sheet,
          title: label,
          closeLabel: close,
          submitLabel: label,
          emailPlaceholder: t("appShell.email"),
          passwordPlaceholder: signingUp
            ? t("appShell.welcome.passwordMin")
            : t("appShell.password"),
          forgotLabel:
            !signingUp && passwordRecovery && connectionReady
              ? t("appShell.auth.forgotPassword")
              : undefined,
          handlePlaceholder: signingUp ? t("appShell.handle") : undefined,
          handleNote: signingUp ? (handleError ?? t("appShell.auth.handlePermanent")) : undefined,
          handleInvalid: handleError !== null,
          handleSuggestion: signingUp && !handleEdited ? handle : undefined,
          avatarCaption: signingUp ? t("appShell.welcome.avatarCaption") : undefined,
          avatar: signingUp ? draftAvatar.composed : undefined,
          email,
          minPasswordLength: signingUp ? 8 : 1,
          busy: submitting,
          error: error?.field === null ? error.message : undefined,
          notice: notice ?? undefined,
        };
      }
      case "recovery": {
        const unavailable = passwordRecovery !== "enabled";
        return {
          kind: "recovery",
          title: t("appShell.resetPassword"),
          closeLabel: close,
          step: unavailable ? "unavailable" : recoveryStep,
          message: unavailable
            ? t("appShell.recovery.disabledReset")
            : recoveryStep === "email"
              ? t("appShell.recovery.intro")
              : t("appShell.recovery.codeSent"),
          email: recoveryEmail,
          emailPlaceholder: t("appShell.email"),
          sendLabel: t("appShell.recovery.sendCode"),
          codePlaceholder: t("appShell.recovery.codePlaceholder"),
          passwordPlaceholder: t("appShell.recovery.newPassword"),
          confirmPlaceholder: t("appShell.confirmPassword"),
          resetLabel: t("appShell.resetPassword"),
          resendLabel: t("appShell.recovery.sendNewCode"),
          backLabel: t("appShell.recovery.backToLogin"),
          busy,
          error: sheetError ?? undefined,
        };
      }
      case "server":
        return {
          kind: "server",
          title: t("appShell.server.address"),
          closeLabel: close,
          url: serverUrl,
          placeholder: "https://",
          saveLabel: busy ? t("common.checking") : t("common.save"),
          busy,
          error: sheetError ?? undefined,
        };
    }
  };

  const failure = connectionStatus ? describeConnectionFailure(connectionStatus) : null;
  const state = {
    dark: appearance.theme === "slack-dark",
    brand: "Silverhand",
    headline: t("appShell.welcome.headline"),
    headlineAccent: t("appShell.welcome.headlineAccent"),
    scene: {
      channel: "launch",
      messages: [
        { name: "Leo", text: t("appShell.welcome.leoSays"), avatar: faces.leo },
        { name: "Doge", text: t("appShell.welcome.dogeSays"), avatar: faces.doge },
      ],
      own: t("appShell.welcome.youSay"),
      typist: faces.typist,
    },
    logInLabel: t("appShell.auth.logIn"),
    createAccountLabel: signupEnabled ? t("appShell.auth.createAccountButton") : undefined,
    privacy: { label: t("appShell.privacyPolicy"), url: PRIVACY_POLICY_URL },
    server: showServerAddress
      ? {
          url: serverUrl
            ? serverUrl.replace(/^https?:\/\//, "").replace(/\/$/, "")
            : t("appShell.server.notSet"),
          label: t("appShell.server.change"),
        }
      : undefined,
    connection: connectionChecking
      ? ({ kind: "checking", label: t("appShell.server.checking") } as const)
      : failure
        ? ({
            kind: "failed",
            ...failure,
            technicalLabel: t("common.technicalDetails"),
            retryLabel: onRetry ? t("common.retry") : undefined,
          } as const)
        : undefined,
    enabled: connectionReady,
    sheet: sheetState(),
  };
  const serialized = JSON.stringify(state);

  // Every push carries the whole screen. Native reads the fields' values only
  // when a sheet opens, so echoes of what the person typed never overwrite it.
  useEffect(() => {
    void screen.show(JSON.parse(serialized));
  }, [screen, serialized, locale]);

  return <div className="h-dvh w-full bg-bg-base" />;
}
