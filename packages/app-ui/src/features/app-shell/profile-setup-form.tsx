import { useId, useState, type FormEvent } from "react";
import { SpinnerIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { AuthSplitLayout } from "./auth-split-layout";
import { DraftAvatarCard } from "@/features/avatar-maker/components/draft-avatar-card";
import { useDraftAvatar } from "@/features/avatar-maker/hooks/use-draft-avatar";
import { rejectDisplayName } from "@/features/identity/display-name";
import { nameFormatMessage } from "@/features/identity/use-name-availability";
import { t } from "@/lib/i18n";

export interface ProfileSetupValues {
  displayName: string;
  username: string;
  avatar: File | null;
  /** Set when `avatar` was composed in the avatar maker rather than uploaded. */
  design: string | null;
}

export interface ProfileSetupFormProps {
  pending: boolean;
  error: string | null;
  onSubmit: (values: ProfileSetupValues) => void;
  onRetry?: () => void;
}

export function ProfileSetupForm({
  pending,
  error,
  onSubmit,
  onRetry,
}: ProfileSetupFormProps): JSX.Element {
  const id = useId();
  const [displayName, setDisplayName] = useState("");
  const [username, setUsername] = useState("");
  const [submitted, setSubmitted] = useState(false);
  const draft = useDraftAvatar();

  const displayNameError = submitted ? rejectDisplayName(displayName.trim()) : null;
  const usernameError = submitted
    ? username.trim()
      ? nameFormatMessage("agent", username.trim())
      : t("appShell.handleRequired")
    : null;

  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (pending) {
      return;
    }
    setSubmitted(true);
    if (
      rejectDisplayName(displayName.trim()) ||
      !username.trim() ||
      nameFormatMessage("agent", username.trim())
    ) {
      return;
    }
    const upload = await draft.resolve();
    onSubmit({
      displayName: displayName.trim(),
      username: username.trim(),
      avatar: upload?.file ?? null,
      design: upload?.design ?? null,
    });
  };

  return (
    <AuthSplitLayout>
      <form
        onSubmit={(event) => void submit(event)}
        data-auth-form
        className="flex w-full max-w-90 flex-col gap-4"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        <h1 className="mb-2 text-3xl font-semibold tracking-tight">
          {t("appShell.profile.setUp")}
        </h1>
        <DraftAvatarCard draft={draft} disabled={pending} />
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${id}-display-name`} required>
            {t("appShell.displayName")}
          </Label>
          <Input
            id={`${id}-display-name`}
            autoFocus
            autoComplete="name"
            placeholder="Ada Lovelace"
            value={displayName}
            disabled={pending}
            onChange={(event) => setDisplayName(event.target.value)}
            aria-invalid={!!displayNameError}
            aria-describedby={displayNameError ? `${id}-display-error` : undefined}
          />
          {displayNameError ? (
            <p id={`${id}-display-error`} role="alert" className="text-xs text-error">
              {displayNameError}
            </p>
          ) : null}
        </div>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${id}-username`} required>
            {t("appShell.handle")}
          </Label>
          <Input
            id={`${id}-username`}
            autoComplete="username"
            autoCapitalize="none"
            spellCheck={false}
            placeholder="ada"
            value={username}
            disabled={pending}
            onChange={(event) => setUsername(event.target.value)}
            aria-invalid={!!usernameError}
            aria-describedby={usernameError ? `${id}-username-error` : undefined}
          />
          {usernameError ? (
            <p id={`${id}-username-error`} role="alert" className="text-xs text-error">
              {usernameError}
            </p>
          ) : null}
        </div>
        {error ? (
          <p role="alert" className="text-xs text-error">
            {error}
          </p>
        ) : null}
        {onRetry ? (
          <Button type="button" variant="primary" className="mt-2 w-full" onClick={onRetry}>
            {t("common.retry")}
          </Button>
        ) : (
          <Button type="submit" variant="primary" className="mt-2 w-full" disabled={pending}>
            {pending ? <SpinnerIcon className="size-4 animate-spin" aria-hidden /> : null}
            {t("common.continue")}
          </Button>
        )}
      </form>
    </AuthSplitLayout>
  );
}
