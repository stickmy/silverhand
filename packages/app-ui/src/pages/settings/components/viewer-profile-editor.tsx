import { useId, useState, type FormEvent, type KeyboardEvent, type ReactNode } from "react";
import { SpinnerIcon, UserIcon } from "@/icons";
import { validateAvatarFile, type UserDto } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import { AvatarMakerGlyph } from "@/features/avatar-maker/components/avatar-maker-glyph";
import { AvatarPickerButton } from "@/components/avatar-picker-button";
import { AVATAR_PX } from "@/components/avatar-scale";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AvatarMakerSheet } from "@/features/avatar-maker/components/avatar-maker-sheet";
import { avatarDesignField } from "@/features/avatar-maker/model/avatar-design-wire";
import { readDisplayNameDraft } from "@/features/identity/display-name";
import {
  useUpdateProfile,
  useUploadAvatar,
  useViewerAvatarObjectUrl,
} from "../../../features/session/hooks";
import { t } from "@/lib/i18n";
import { SettingsGroup, SettingsSection } from "./settings-section";

export function ViewerProfileEditor({
  user,
  email,
}: {
  user: UserDto;
  email?: ReactNode;
}): JSX.Element {
  const update = useUpdateProfile();
  const upload = useUploadAvatar();
  const avatarObjectUrl = useViewerAvatarObjectUrl();
  const nameFieldId = useId();
  const nameErrorId = useId();

  const [draftName, setDraftName] = useState<string | null>(null);
  const [avatarError, setAvatarError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [makerOpen, setMakerOpen] = useState(false);

  const nameValue = draftName ?? user.display_name;

  const nameDraft = readDisplayNameDraft(draftName, user.display_name);
  const canSave = nameDraft.kind === "ready" && !update.isPending;

  const nameRejection = nameDraft.kind === "rejected" ? nameDraft.message : null;

  const onPickAvatar = async (file: File): Promise<void> => {
    setAvatarError(null);
    const validation = validateAvatarFile(file);
    if (validation) {
      setAvatarError(validation.message);
      return;
    }
    try {
      await upload.mutateAsync({ file });
    } catch (err) {
      setAvatarError(formatAvatarMutationError(err));
    }
  };

  const onSubmit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (nameDraft.kind !== "ready" || update.isPending) return;
    setSaveError(null);
    try {
      await update.mutateAsync({ display_name: nameDraft.value });
      // Back to pristine: the fresh dto is now the field's value, so the
      // button disables itself without a second source of truth.
      setDraftName(null);
    } catch (err) {
      setSaveError(formatProfileSaveError(err));
    }
  };

  const onNameKeyDown = (event: KeyboardEvent<HTMLInputElement>): void => {
    // Escape discards the draft — the counterpart of Enter submitting
    // through the form. Neither blur nor navigation saves.
    if (event.key === "Escape") {
      event.preventDefault();
      setDraftName(null);
      setSaveError(null);
    }
  };

  const hasAvatar = user.avatar_url != null;

  return (
    <form onSubmit={(event) => void onSubmit(event)}>
      <SettingsSection label={t("settings.account.profile")}>
        <SettingsGroup className="gap-4 p-4">
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-3.5">
              <AvatarPickerButton
                size={AVATAR_PX["3xl"]}
                onPick={(file) => void onPickAvatar(file)}
                onMake={() => setMakerOpen(true)}
                makeIcon={<AvatarMakerGlyph />}
                disabled={upload.isPending}
                ariaLabel={
                  hasAvatar ? t("settings.profile.changeAvatar") : t("settings.profile.addAvatar")
                }
                className="border border-border-subtle"
              >
                <span data-avatar-ground className="grid size-full place-items-center">
                  {avatarObjectUrl ? (
                    <img
                      src={avatarObjectUrl}
                      alt={t("settings.profile.yourAvatar")}
                      className="size-full object-cover"
                    />
                  ) : (
                    <UserIcon className="size-7 text-text-muted" />
                  )}
                </span>
              </AvatarPickerButton>
              <div className="min-w-0">
                <p className="truncate font-serif text-base font-semibold leading-tight text-text-primary">
                  {user.display_name}
                </p>
                <p className="mt-0.5 truncate font-mono text-xs text-text-muted">@{user.name}</p>
              </div>
            </div>
            <AvatarMakerSheet
              subject="me"
              open={makerOpen}
              onOpenChange={setMakerOpen}
              onSave={async ({ config, png }) => {
                setAvatarError(null);
                await upload.mutateAsync({ file: png, design: avatarDesignField(config) });
              }}
            />
            {avatarError ? (
              <p
                role="alert"
                className="text-(length:--text-meta) leading-snug text-error"
                data-testid="human-avatar-error"
              >
                {avatarError}
              </p>
            ) : null}
          </div>

          <div className="flex flex-col gap-1.5">
            <label
              htmlFor={nameFieldId}
              className="text-sm font-semibold leading-snug text-text-primary"
            >
              {t("appShell.displayName")}
            </label>
            <Input
              id={nameFieldId}
              value={nameValue}
              disabled={update.isPending}
              aria-invalid={nameRejection ? true : undefined}
              aria-describedby={nameRejection ? nameErrorId : undefined}
              onChange={(event) => setDraftName(event.target.value)}
              onKeyDown={onNameKeyDown}
              className="h-8"
            />
            {nameRejection ? (
              <p id={nameErrorId} role="alert" className="text-xs text-error">
                {nameRejection}
              </p>
            ) : null}
          </div>

          {email}

          <div className="flex flex-col items-start gap-2 pt-1">
            <Button type="submit" variant="primary" size="sm" disabled={!canSave}>
              {update.isPending ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
              {t("settings.profile.save")}
            </Button>
            {saveError ? (
              <p role="alert" className="text-xs text-error">
                {saveError}
              </p>
            ) : null}
          </div>
        </SettingsGroup>
      </SettingsSection>
    </form>
  );
}

/**
 * Map a failed `PATCH /api/me` to a one-line user-facing message. Only
 * the arms a viewer can act on are named; everything else keeps the
 * status code so a report is diagnosable.
 */
export function formatProfileSaveError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.kind === "backend_unreachable") {
      return t("settings.profile.unreachable");
    }
    switch (err.status) {
      case 400:
        return t("settings.profile.nameRejected");
      case 401:
        return t("settings.profile.signInExpired");
      default:
        return t("settings.profile.saveFailedStatus", { status: err.status });
    }
  }
  return t("settings.profile.saveFailed");
}

/**
 * Map a failed `/api/me/avatar` mutation to a one-line user-facing
 * message. Mirrors `formatAvatarMutationError` in `EditableAgentAvatar`
 * but localized for the user surface.
 */
export function formatAvatarMutationError(err: unknown): string {
  if (err instanceof ApiError) {
    const code = parseErrorCode(err.body);
    switch (code) {
      case "avatar_too_large":
        return t("settings.profile.avatarTooLarge");
      case "avatar_unsupported_type":
        return t("settings.profile.avatarType");
      case "avatar_missing":
        return t("settings.profile.avatarEmpty");
      case "avatar_storage_failed":
        return t("settings.profile.avatarStorage");
      default:
        break;
    }
    switch (err.status) {
      case 401:
        return t("settings.profile.signInExpired");
      case 403:
        return t("settings.profile.noPermission");
      case 404:
        return t("settings.profile.gone");
      default:
        return t("settings.profile.avatarFailedStatus", { status: err.status });
    }
  }
  return t("settings.profile.avatarFailed");
}

function parseErrorCode(body: string): string | null {
  if (!body) return null;
  try {
    const parsed = JSON.parse(body) as { code?: unknown };
    return typeof parsed.code === "string" ? parsed.code : null;
  } catch {
    return null;
  }
}
