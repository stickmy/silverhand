import { Fragment, useEffect, useState, type FormEvent, type ReactNode } from "react";
import { useMutation } from "@tanstack/react-query";
import { ArchiveIcon, LeaveIcon, LockIcon, SpinnerIcon, UnarchiveIcon, UnlockIcon } from "@/icons";
import type { ChannelLeaveBlockedReason, ConversationVisibility } from "@silverhand/shared";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { t } from "@/lib/i18n";

const SECTION_TITLE_CLASS = "text-xs font-semibold uppercase tracking-wide text-text-secondary";
const ROW_CLASS = "flex min-h-10 w-full items-center gap-2.5 px-3 py-2 text-left text-xs";
const ROW_BUTTON_CLASS = `${ROW_CLASS} transition-colors hover:bg-bg-hover focus-outline disabled:pointer-events-none disabled:text-text-muted`;
const ROW_ICON_CLASS = "size-3.5 shrink-0 text-text-secondary";

/** Client-side cap. Keep in sync with the backend rename schema. */
const MAX_TITLE_LENGTH = 128;

interface ChannelSettingsDetailsProps {
  notifications?: ReactNode;
  /** The channel's current title (no `#` prefix — that's presentational). */
  currentTitle: string;
  /** Persist the trimmed new title. Rejects with a surfaced message.
   *  Absent → the rename section is hidden (e.g. a closed channel). */
  onRename?: (title: string) => Promise<void>;
  /** Current stored visibility, from the channel-detail carrier. Drives the
   *  glyph + the Public/Private label + the leave consequence copy. */
  visibility?: ConversationVisibility;
  /** Whether the viewer may toggle visibility (workspace admin). Fail-closed:
   *  the visibility section is hidden unless this is `true` AND `onSetVisibility`
   *  is wired AND `visibility` resolved. */
  canUpdateVisibility?: boolean;
  /** Persist the new visibility (its own exactly-one-field PATCH). Async;
   *  surfaces rejection inline and keeps the panel open. */
  onSetVisibility?: (visibility: ConversationVisibility) => Promise<void>;
  isArchived: boolean;
  /** Toggle archive state. Absent → the archive section is hidden. */
  onToggleArchive?: (archived: boolean) => Promise<void>;
  archivePending?: boolean;
  /** Self-leave the channel. Async; surfaces rejection inline. Absent → the
   *  leave section is hidden. Present but `!canLeave` → disabled + reason. */
  onLeave?: () => Promise<void>;
  /** Whether the viewer may leave, from the carrier. Fail-closed default. */
  canLeave?: boolean;
  /** Stable reason the viewer may NOT leave (renderer owns the copy). */
  leaveBlockedReason?: ChannelLeaveBlockedReason | null;
}

/** Human copy for a leave-blocked reason. The backend emits only the stable
 *  code; UI text is never outsourced to the backend. `null` → no line (either
 *  leave is allowed, or a code that shouldn't reach a member, e.g. not_member). */
function leaveBlockedCopy(reason: ChannelLeaveBlockedReason | null | undefined): string | null {
  switch (reason) {
    case "channel_owner":
      return t("conversationShell.asTheOwnerYou");
    case "last_human_member":
      return t("conversationShell.asTheLastMember");
    case "channel_archived":
      return t("conversationShell.restoreTheChannelBefore");
    default:
      return null;
  }
}

function errorText(err: unknown): string | null {
  if (err == null) return null;
  return err instanceof Error ? err.message : String(err);
}

export function ChannelSettingsDetails({
  notifications,
  currentTitle,
  onRename,
  visibility,
  canUpdateVisibility = false,
  onSetVisibility,
  isArchived,
  onToggleArchive,
  archivePending = false,
  onLeave,
  canLeave = false,
  leaveBlockedReason = null,
}: ChannelSettingsDetailsProps): JSX.Element {
  const [title, setTitle] = useState(currentTitle);
  // Leave is an inline two-step confirm; the rest of each action's lifecycle
  // (pending, failure) is the mutation's own state.
  const [leaveConfirm, setLeaveConfirm] = useState(false);

  const rename = useMutation({ mutationFn: (next: string) => onRename!(next) });
  const setVis = useMutation({
    mutationFn: (next: ConversationVisibility) => onSetVisibility!(next),
  });
  const archive = useMutation({ mutationFn: (next: boolean) => onToggleArchive!(next) });
  const leave = useMutation({ mutationFn: () => onLeave!() });

  const saving = rename.isPending;
  const error = errorText(rename.error);
  const visSaving = setVis.isPending;
  const visError = errorText(setVis.error);
  const archiveError = errorText(archive.error);
  // After a successful leave the host navigates away; until then the button
  // stays busy so the confirm can't be sent twice.
  const leaving = leave.isPending || leave.isSuccess;
  const leaveError = errorText(leave.error);

  const resetRename = rename.reset;
  const resetVis = setVis.reset;
  const resetLeave = leave.reset;

  // The host remounts on channel changes; a successful rename refreshes the form.
  useEffect(() => {
    setTitle(currentTitle);
    setLeaveConfirm(false);
    resetRename();
    resetVis();
    resetLeave();
  }, [currentTitle, resetRename, resetVis, resetLeave]);

  const trimmed = title.trim();
  const dirty = trimmed !== currentTitle.trim();
  const saveDisabled = !onRename || saving || trimmed === "" || !dirty;

  const handleSubmit = (e: FormEvent): void => {
    e.preventDefault();
    if (!onRename || saveDisabled) return;
    rename.mutate(trimmed);
  };

  const handleToggleArchive = (): void => {
    if (!onToggleArchive || archivePending) return;
    archive.mutate(!isArchived);
  };

  const isPrivate = visibility === "private";
  const nextVisibility: ConversationVisibility = isPrivate ? "public" : "private";
  const showVisibility = !!onSetVisibility && canUpdateVisibility && visibility !== undefined;

  const handleToggleVisibility = (): void => {
    if (!onSetVisibility || visSaving) return;
    setVis.mutate(nextVisibility);
  };

  // A failure keeps the confirm open so the error shows in context and
  // Cancel / retry stay reachable.
  const handleConfirmLeave = (): void => {
    if (!onLeave || leaving) return;
    leave.mutate();
  };

  const blockedCopy = leaveBlockedCopy(leaveBlockedReason);

  const renameNode = onRename ? (
    <form className="flex flex-col gap-2" onSubmit={handleSubmit}>
      <div className="flex items-center gap-2">
        <Input
          id="channel-settings-name"
          aria-label={t("conversationShell.channelName")}
          value={title}
          maxLength={MAX_TITLE_LENGTH}
          spellCheck={false}
          disabled={saving}
          onChange={(e) => setTitle(e.target.value)}
        />
        <Button type="submit" variant="primary" className="shrink-0" disabled={saveDisabled}>
          {saving ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
          {t("common.save")}
        </Button>
      </div>
      {error ? <p className="text-xs text-error">{error}</p> : null}
    </form>
  ) : null;

  const rows: { key: string; node: JSX.Element }[] = [];

  if (showVisibility) {
    rows.push({
      key: "visibility",
      node: (
        <div className={ROW_CLASS}>
          {isPrivate ? (
            <LockIcon className={ROW_ICON_CLASS} />
          ) : (
            <UnlockIcon className={ROW_ICON_CLASS} />
          )}
          <span className="min-w-0 flex-1">{t("conversationShell.visibility")}</span>
          <span className="text-text-muted">
            {isPrivate ? t("conversationShell.private") : t("conversationShell.public")}
          </span>
          <Button
            type="button"
            variant="neutral"
            size="sm"
            className="text-xs"
            disabled={visSaving}
            onClick={handleToggleVisibility}
          >
            {visSaving ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
            {isPrivate ? t("conversationShell.makePublic") : t("conversationShell.makePrivate")}
          </Button>
        </div>
      ),
    });
  }

  if (onToggleArchive) {
    rows.push({
      key: "archive",
      node: (
        <button
          type="button"
          className={ROW_BUTTON_CLASS}
          disabled={archivePending}
          onClick={handleToggleArchive}
        >
          {archivePending ? (
            <SpinnerIcon className={`${ROW_ICON_CLASS} animate-spin`} />
          ) : isArchived ? (
            <UnarchiveIcon className={ROW_ICON_CLASS} />
          ) : (
            <ArchiveIcon className={ROW_ICON_CLASS} />
          )}
          <span className="min-w-0 flex-1">
            {isArchived
              ? t("conversationShell.restoreChannel")
              : t("conversationShell.archiveChannel")}
          </span>
        </button>
      ),
    });
  }

  if (onLeave) {
    rows.push({
      key: "leave",
      // Confirming expands this row in place with the consequence and
      // Cancel/confirm; the row itself never moves.
      node: canLeave ? (
        <div>
          <button
            type="button"
            className={ROW_BUTTON_CLASS}
            disabled={leaving || leaveConfirm}
            onClick={() => setLeaveConfirm(true)}
          >
            <LeaveIcon className={ROW_ICON_CLASS} />
            <span className="min-w-0 flex-1">{t("conversationShell.leaveChannel")}</span>
          </button>
          {leaveConfirm ? (
            <div className="flex flex-col gap-2.5 px-3 pb-3">
              <div className="rounded-(--control-radius) bg-(--warning-surface) px-3 py-2.5">
                <p className="text-sm leading-relaxed text-text-primary">
                  {isPrivate
                    ? t("conversationShell.youWontBeAble")
                    : t("conversationShell.youCanRejoinThis")}
                </p>
              </div>
              <div className="flex items-center justify-end gap-2">
                <Button
                  variant="cancel"
                  size="sm"
                  disabled={leaving}
                  onClick={() => setLeaveConfirm(false)}
                >
                  {t("common.cancel")}
                </Button>
                <Button
                  variant="destructive-outline"
                  size="sm"
                  disabled={leaving}
                  onClick={handleConfirmLeave}
                >
                  {leaving ? (
                    <SpinnerIcon className="size-3.5 animate-spin" />
                  ) : (
                    <LeaveIcon className="size-3.5" />
                  )}
                  {t("conversationShell.leave")}
                </Button>
              </div>
              {leaveError ? <p className="text-xs text-error">{leaveError}</p> : null}
            </div>
          ) : null}
        </div>
      ) : (
        <button type="button" className={ROW_BUTTON_CLASS} disabled>
          <LeaveIcon className={ROW_ICON_CLASS} />
          <span className="flex min-w-0 flex-1 flex-col gap-0.5">
            {t("conversationShell.leaveChannel")}
            {blockedCopy ? <span className="text-text-muted">{blockedCopy}</span> : null}
          </span>
        </button>
      ),
    });
  }

  return (
    <div className="flex flex-col gap-8">
      <section className="flex flex-col gap-3">
        <h3 className={SECTION_TITLE_CLASS}>{t("conversationShell.name")}</h3>
        {renameNode ?? <p className="text-sm font-medium">{currentTitle}</p>}
        {isArchived ? <p className="mt-2 text-xs text-text-muted">{t("common.archived")}</p> : null}
      </section>
      {notifications}
      {archiveError ? (
        <p role="alert" className="text-xs text-error">
          {archiveError}
        </p>
      ) : null}
      {rows.length > 0 ? (
        <section className="flex flex-col gap-3">
          <h3 className={SECTION_TITLE_CLASS}>{t("conversationShell.channel")}</h3>
          <div
            data-settings-group
            className="flex flex-col divide-y divide-border-subtle overflow-hidden rounded-(--card-radius) border border-border-subtle bg-bg-floating"
          >
            {rows.map((row) => (
              <Fragment key={row.key}>{row.node}</Fragment>
            ))}
          </div>
          {visError ? <p className="text-xs text-error">{visError}</p> : null}
        </section>
      ) : null}
    </div>
  );
}
