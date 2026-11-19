import { TaskBodySkeleton } from "../../../features/tasks/components/task-body-skeleton";
import { useEffect, useState } from "react";
import { EditIcon, OpenExternalIcon } from "@/icons";
import type { TaskDetailDto } from "@silverhand/shared";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  dialogCornerButtonClass,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { MiniIconButton } from "@/features/tasks/components/mini-icon-button";
import { TaskDetailMetaPanel } from "@/pages/task/components/task-detail-meta-panel";
import {
  failureCopy,
  InlineFieldEditor,
  useTaskWriteSlot,
  type EditingField,
} from "@/features/tasks/components/task-detail-shared";
import { Markdown } from "@/features/messages/components/markdown";
import { useTaskDetail } from "@/features/tasks/hooks/hooks";
import { TaskHistorySection } from "@/features/tasks/components/task-history-section";
import {
  classifyTaskDetailFailure,
  resolveTaskDetailView,
  resolveTaskJumpTarget,
  type TaskDetailFailure,
  type TaskDetailView,
  type TaskJumpTarget,
} from "@/features/tasks/model/task-detail";
import { t } from "@/lib/i18n";

export type { TaskJumpTarget } from "@/features/tasks/model/task-detail";

export interface TaskDetailDialogProps {
  /** Task identity to show, or null when the modal is closed. An ID, not a
   *  summary DTO: every entrance (chat chip, board card, cold `?task=<id>`
   *  deep-link) can supply one, so the modal has a SINGLE data source
   *  (#task:1715) instead of painting from a possibly-stale card while the
   *  authoritative read is in flight. */
  taskId: string | null;
  onOpenChange: (open: boolean) => void;
  /** "View in conversation". The surface wires this to its own jump handler; the
   *  dialog does NOT close itself — whoever navigates owns the pointer's fate
   *  (App closes it only when no navigation happened). */
  onJump: (target: TaskJumpTarget) => void;
}

/**
 * Task detail modal — the ONE surface for reading a single task, opened by
 * `?task=<task_id>` in the URL (#task:1715): a task card on either board
 * surface, an inline `#task:<n>` chip in chat, or a cold deep-link all write
 * that one pointer, so there is a single modal instance at app level with a
 * single state. Reads the full body + edit-provenance byline on demand.
 *
 * Because a chip click no longer opens the tasks board first, the modal may be
 * the only task surface on screen. "View in conversation" sits beside the close
 * ✕ in the corner cluster; it does not close the modal from here — the surface
 * that navigates decides what happens to the `?task` pointer, so there is exactly
 * ONE owner for the action (Cindy BLOCKER 3) instead of a close racing a route
 * change. A cold deep-link shows the detail only; there is no jump-to-board
 * affordance (shiki 2026-08-21 — the reveal machinery was retired, #task:2124).
 *
 * WHAT IS RENDERED is one exhaustive decision, `resolveTaskDetailView` — failure
 * tier and "is there a body to paint" are independent axes and both are real
 * (a background refresh can fail while the previous body is still cached). The
 * `switch` below is total over that union, so no branch order can swallow a tier.
 *
 * A read that doesn't resolve gets a COMPACT single-column shell, not the wide
 * two-column one: the wide shape exists to seat a description beside a metadata
 * column, and neither exists yet. The URL pointer is never stripped on failure —
 * the user clicked that reference, so it stays addressable until they close it
 * (same principle as `SideUnresolvedPlaceholder`; the corner ✕ is the explicit
 * close, so the panel adds no second Close button).
 *
 * Editing is IN PLACE and PER FIELD: a pencil beside the title / description
 * turns just that region into an inline editor (no second modal, no global
 * edit mode). Each editor saves on blur and submits ONLY its own field as a
 * plain last-write-wins update — the omitted field stays untouched. While a
 * field is being edited the modal refuses close requests (an outside-click / ✕
 * only fires the blur-save; a validation/request failure keeps the draft in the
 * open editor) and the jump icon is hidden. "View in conversation" is the modal
 * close ✕'s TWIN — same corner-button skin (and poster-sticker), sitting
 * in the top-right action cluster just left of the close — consuming the
 * viewer-resolved `nav` discriminant (source → source message, else →
 * task-created event; hidden entirely when nav is `unavailable`).
 */
export function TaskDetailDialog({
  taskId,
  onOpenChange,
  onJump,
}: TaskDetailDialogProps): JSX.Element {
  const open = taskId !== null;
  const detailQuery = useTaskDetail(taskId, { enabled: open });
  const failure = detailQuery.isError ? classifyTaskDetailFailure(detailQuery.error) : null;
  const view = resolveTaskDetailView({ failure, detail: detailQuery.data });
  // The body this view paints, if any. `not_found` deliberately paints none even
  // when a stale one is still cached — the server has just disowned it.
  const painted =
    view.kind === "body" || view.kind === "body_with_retry" || view.kind === "body_stale"
      ? view.detail
      : null;

  const [editingField, setEditingField] = useState<EditingField>(null);
  // Snap back to read mode whenever the modal opens onto a different task
  // (or closes) so a stale inline draft never bleeds across tasks. Note this
  // keys on the ID, NOT the live detail — a task_changed refetch of the SAME
  // task must not reset an open editor and wipe the unsubmitted draft.
  useEffect(() => {
    setEditingField(null);
  }, [taskId]);

  // Corner actions only exist once there is a body to act on: before that there
  // is no `nav` discriminant to branch on, and a disabled placeholder button
  // would promise a destination we can't name yet. Both are also hidden while a
  // field is being edited — a click would blur-save and then navigate away,
  // unmounting the editor and losing the draft if the write failed.
  const jumpTarget = painted ? resolveTaskJumpTarget(painted) : null;
  const actionsVisible = open && painted !== null && editingField === null;
  const resolved = painted !== null;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // Blur-save is async: an outside-click / ✕ blurs the field (firing the
        // save) AND asks the dialog to close. (Escape doesn't reach here — the
        // editor cancels it and preventDefaults, so Radix never gets it.)
        // Closing now would unmount before the write settles — a failure would
        // lose the inline error and the draft. So refuse close requests while a
        // field is being edited; the click still triggers blur-save, and
        // success drops `editingField` back to null, after which a second close
        // lands.
        if (!next && editingField !== null) return;
        onOpenChange(next);
      }}
    >
      <DialogContent
        // Widest dialog after workspace settings: the body carries prose
        // (description, edit fields) that reads badly in a 512px column.
        // Explicit px instead of a Tailwind step because this sits between
        // `max-w-2xl` and `max-w-3xl`.
        //
        // 900, not 800 (shiki 2026-07-29 "正文的地方也变小了"): the 240px
        // metadata column is a real tax on the reading column — at 800 the main
        // column was 520px, so the description had ~494px, down from ~734px
        // pre-sidebar. Every px added here goes to the prose (the sidebar is
        // fixed), and 900 keeps the modal at ~70% of the default 1280 window.
        // Restoring the FULL pre-sidebar width is deliberately not the goal:
        // 734px is ~103ch at the default prose size, well past readable.
        //
        // HEIGHT is a band, not one cap (shiki 2026-07-29). Floor 300px: the
        // sidebar's rows used to be the only floor, so a short task's modal
        // height moved with how many optional rows (Edited / Closed) happened to
        // exist — the same task read as two different shapes. Ceiling 70vh
        // instead of 80vh: 80 on a tall window produced a column of mostly empty
        // space under a short description. One value is fixed and the other
        // viewport-relative on purpose — the floor is a shape guarantee, the
        // ceiling is "leave the window visible around me". They only cross below
        // a ~430px-tall window, where the app has no usable layout anyway.
        //
        // Not-yet-resolved states drop BOTH: no prose, no metadata column, so
        // the wide band would frame a sentence in a mostly-empty 900×300 box.
        className={
          resolved
            ? "flex max-h-[70vh] min-h-75 max-w-225 flex-col gap-0 p-0"
            : "flex max-w-105 flex-col gap-0 p-0"
        }
        // This dialog owns its padding (`p-0` + a bordered header at `px-5
        // py-4`), so the cluster keeps hugging the corner: its band is the
        // metadata row, not a `p-6` title line.
        cornerBand="top"
        cornerActions={
          actionsVisible && jumpTarget ? (
            <button
              type="button"
              data-task-view-jump
              data-sticker
              data-sticker-size="md"
              data-sticker-tone="neutral"
              title={t("taskPage.viewInConversation")}
              aria-label={t("taskPage.viewInConversation")}
              onClick={() => onJump(jumpTarget)}
              className={dialogCornerButtonClass}
            >
              <OpenExternalIcon className="size-4" aria-hidden />
            </button>
          ) : null
        }
      >
        <TaskDetailContent
          view={view}
          editingField={editingField}
          onEditField={setEditingField}
          onStopEditing={() => setEditingField(null)}
          onRetry={() => void detailQuery.refetch()}
        />
      </DialogContent>
    </Dialog>
  );
}

/**
 * Total `switch` over the resolved view — the only place that decides what the
 * modal shows. Adding a state to `TaskDetailView` breaks this exhaustively
 * instead of falling into whichever branch happens to come first.
 */
function TaskDetailContent({
  view,
  editingField,
  onEditField,
  onStopEditing,
  onRetry,
}: {
  view: TaskDetailView;
  editingField: EditingField;
  onEditField: (field: Exclude<EditingField, null>) => void;
  onStopEditing: () => void;
  onRetry: () => void;
}): JSX.Element | null {
  switch (view.kind) {
    case "loading":
      return <TaskDetailStatePanel failure={null} />;
    case "not_found":
      return <TaskDetailStatePanel failure="not_found" />;
    case "unreachable_empty":
      return <TaskDetailStatePanel failure="unreachable" />;
    case "transient_empty":
      return <TaskDetailStatePanel failure="transient" onRetry={onRetry} />;
    case "body":
    // `body_stale` is the SAME body: the connection is down, so the last read
    // is the best available truth and the one app-status banner already says
    // the connection is down. A second copy of that notice here would be a
    // second surface claiming one fact (Alice, #task:1462).
    case "body_stale":
      return (
        <TaskDetailBody
          detail={view.detail}
          editingField={editingField}
          onEditField={onEditField}
          onStopEditing={onStopEditing}
        />
      );
    case "body_with_retry":
      return (
        <>
          {/* The refresh failed over a body we still hold. Nothing re-reads it
              on its own (`retry: 1`, no focus refetch), so the Retry has to be
              reachable here — but it stays a single muted line: the body below
              is still readable, this is a footnote about its freshness. */}
          <div className="flex items-center gap-3 border-b border-border-subtle px-5 py-2 text-xs text-text-muted">
            <span>{t("taskPage.showingTheLastLoaded")}</span>
            <button
              type="button"
              onClick={onRetry}
              className="ml-auto shrink-0 rounded px-1 py-0.5 font-medium text-text-secondary underline-offset-2 hover:text-text-primary hover:underline focus-outline"
            >
              {t("common.retry")}
            </button>
          </div>
          <TaskDetailBody
            detail={view.detail}
            editingField={editingField}
            onEditField={onEditField}
            onStopEditing={onStopEditing}
          />
        </>
      );
  }
}

// ── Unresolved states (loading + the three failure tiers) ──────────

function TaskDetailStatePanel({
  failure,
  onRetry,
}: {
  /** null → still loading. */
  failure: TaskDetailFailure | null;
  /** Only the transient tier gets one; the others have no local recovery. */
  onRetry?: () => void;
}): JSX.Element {
  if (failure === null) {
    return (
      <div>
        <DialogTitle className="sr-only">{t("taskPage.taskDetail")}</DialogTitle>
        <DialogDescription className="sr-only">{t("taskPage.loadingTask")}</DialogDescription>
        <TaskBodySkeleton />
      </div>
    );
  }
  const copy = failureCopy()[failure];
  return (
    <div className="flex flex-col gap-1.5 px-5 py-6 pr-12">
      <DialogTitle className="text-sm font-semibold leading-snug text-text-primary">
        {copy.header}
      </DialogTitle>
      <DialogDescription className="text-sm text-text-muted">{copy.body}</DialogDescription>
      {/* Retry ONLY for the transient tier: the query client is `retry: 1` with
          no focus refetch, so nothing else re-reads this — without a button the
          pointer is stuck. `not_found` is terminal and `unreachable` recovers
          through the app-status banner. */}
      {failure === "transient" && onRetry ? (
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="mt-2 self-start"
          onClick={onRetry}
        >
          {t("common.retry")}
        </Button>
      ) : null}
    </div>
  );
}

// ── Body ───────────────────────────────────────────────────────────

function TaskDetailBody({
  detail,
  editingField,
  onEditField,
  onStopEditing,
}: {
  /** The AUTHORITATIVE read. The body only mounts once it resolves, so there is
   *  no "paint the card's summary while the real values arrive" window — that
   *  window used to need a per-field merge, and a per-field `??` silently
   *  resurrects a stale value the moment the authoritative one becomes null. */
  detail: TaskDetailDto;
  editingField: EditingField;
  onEditField: (field: Exclude<EditingField, null>) => void;
  onStopEditing: () => void;
}): JSX.Element {
  // The metadata sidebar's two writes share ONE dialog-local write slot with
  // the title/content editor (`useTaskWriteSlot`): while any of them is open or
  // in flight the others go inert. Without that, clicking Status during a title
  // edit fires the blur-save AND a status write in the same interaction, and
  // `useEditTask`'s whole-object cache write can land last and clobber the newer
  // status (Cindy review, shiki 2026-07-29 "ui 上可以加状态来阻塞用户"). Local slot
  // only — no lock, no revision, no multi-writer protocol.
  const slot = useTaskWriteSlot(detail.id, editingField);
  const { busy } = slot;

  return (
    <>
      <DialogHeader className="border-b border-border-subtle px-5 py-4">
        {/* Header is the TITLE line only — number, status and the edit byline
            moved into the metadata sidebar, where they sit beside the rest of
            the task's facts instead of stacking chrome above the title.
            `pr-28` reserves the corner cluster, which shares this top band:
            three 28px sticker boxes (poster adds a 2px border to the 24px
            `p-1`+`size-4` box) with `gap-1.5` and `right-3` reach 108px from
            the dialog's right edge, and the header's own `px-5` only accounts
            for 20 of those — so anything under 88px lets the title's edit
            pencil slide under the buttons (shiki 2026-07-29 "编辑 icon 和右侧
            三个 action button 有一些重叠"). 112px keeps a margin as the cluster
            grows. */}
        <div className="pr-28">
          {editingField === "title" ? (
            <>
              {/* Radix needs a DialogTitle for a11y even while the visible title
                  is swapped for the inline editor. */}
              <DialogTitle className="sr-only">{t("taskPage.editTaskTitle")}</DialogTitle>
              <InlineFieldEditor
                taskId={detail.id}
                field="title"
                initial={detail.title}
                onDone={onStopEditing}
                onCancel={onStopEditing}
              />
            </>
          ) : (
            <div className="flex items-start gap-2">
              <DialogTitle className="flex-1 text-base leading-snug wrap-anywhere">
                {detail.title}
              </DialogTitle>
              {!busy ? (
                <MiniIconButton
                  icon={EditIcon}
                  label={t("taskPage.editTitle")}
                  className="mt-0.5"
                  onClick={() => onEditField("title")}
                />
              ) : null}
            </div>
          )}
        </div>
        <DialogDescription className="sr-only">{t("taskPage.taskDetail")}</DialogDescription>
      </DialogHeader>

      {/* Two columns, each scrolling on its own: a long description scrolls
          without dragging the metadata out of view, while the metadata stays
          put. Load / failure states never reach here — they get their own
          compact shell, so this column is always real content. */}
      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col gap-6 overflow-y-auto px-5 py-4">
          <TaskHistorySection taskId={detail.id} />

          {editingField === "content" ? (
            <InlineFieldEditor
              taskId={detail.id}
              field="content"
              initial={detail.content}
              onDone={onStopEditing}
              onCancel={onStopEditing}
            />
          ) : (
            <div className="flex items-start gap-2">
              {/* Reading measure, in `ch` so it tracks the user's own prose size
                  (`--appearance-markdown-font-size`) instead of a fixed px.
                  76ch ≈ the widened column minus the pencil, so the extra width
                  from 800→900 lands in the text rather than as trailing slack —
                  while still stopping short of the ~103ch the single-column
                  modal used to run at. */}
              <div className="min-w-0 max-w-[76ch] flex-1">
                {detail.content.trim() ? (
                  <Markdown content={detail.content} taskReferences="raw" />
                ) : (
                  <p className="text-sm italic text-text-muted">{t("taskPage.noDescription")}</p>
                )}
              </div>
              {!busy ? (
                <MiniIconButton
                  icon={EditIcon}
                  label={t("taskPage.editDescription")}
                  className="mt-0.5"
                  onClick={() => onEditField("content")}
                />
              ) : null}
            </div>
          )}
        </div>

        <TaskDetailMetaPanel view={detail} {...slot} />
      </div>
    </>
  );
}
