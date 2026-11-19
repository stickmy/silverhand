import { Fragment, useEffect, useState } from "react";
import { paneChrome } from "@/components/layout/pane-chrome";
import { EditIcon, OpenExternalIcon, TaskIcon } from "@/icons";
import type { TaskDetailDto } from "@silverhand/shared";
import { Button } from "@/components/ui/button";
import { MiniIconButton } from "@/features/tasks/components/mini-icon-button";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { Markdown } from "@/features/messages/components/markdown";
import {
  failureCopy,
  InlineFieldEditor,
  useTaskMetaRows,
  useTaskWriteSlot,
  type EditingField,
} from "@/features/tasks/components/task-detail-shared";
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

export interface TaskDetailPanelProps {
  /** Task identity to show. Never a summary DTO — the pane reads the
   *  authoritative detail itself (#task:1715), so a chip's stale card can't
   *  paint it. Non-null: the branch that renders this pane already gated on
   *  `?task=<id>` being present. */
  taskId: string;
  /** Omitted when the pane is a pushed page whose navigation bar owns Back. */
  onClose?: () => void;
  /** Adds "View in conversation" to the header. */
  onJump?: (target: TaskJumpTarget) => void;
  /** Report whether an inline field editor is open (an UNSETTLED edit). The host
   *  reads this to gate its single request-close owner: a ✕ / ⌘W while editing
   *  must not unmount the pane before the blur-save settles (#task:1721). */
  onEditingChange: (editing: boolean) => void;
}

/**
 * The conversation-anchored task detail — the OVERLAY SIDE PANE half of the
 * task-detail split (#task:1721). Opened by `?task=<id>` while the reader is in
 * a channel chat, it sits beside the conversation instead of covering it, so a
 * task can be read and edited without leaving the message it was referenced
 * from. Every OTHER entry (board card, cold deep-link) keeps the
 * app-level `TaskDetailDialog` modal — the two share ALL their substance
 * (`taskDetailShared`), differing only in chrome and metadata layout, so they
 * can never drift on how a task reads, edits, or fails.
 *
 * The header carries a static "Task" label with the task number as its subtitle
 * plus close (shiki 2026-07-29) — the same title/subtitle idiom as the
 * attachments pane. The task's own title lives in the body with its edit pencil,
 * so the header doesn't repeat it. Metadata reflows to the PANE's own width — one column when narrow,
 * two when wide — via a container query (`.task-pane` / `.task-pane-meta`), not
 * the viewport, so a dragged-narrow pane stacks and a dragged-wide one saves
 * vertical height.
 *
 * WHAT IS RENDERED is the same exhaustive `resolveTaskDetailView` decision the
 * modal uses; the `switch` below is total over that union. Unlike the modal it
 * paints its states in plain markup — a side pane has no Radix Dialog to satisfy
 * with a `DialogTitle`/`DialogDescription`.
 */
export function TaskDetailPanel({
  taskId,
  onClose,
  onJump,
  onEditingChange,
}: TaskDetailPanelProps): JSX.Element {
  const detailQuery = useTaskDetail(taskId, { enabled: true });
  const failure = detailQuery.isError ? classifyTaskDetailFailure(detailQuery.error) : null;
  const view = resolveTaskDetailView({ failure, detail: detailQuery.data });
  // The body this view paints, if any. `not_found` deliberately paints none even
  // when a stale one is still cached — the server has just disowned it.
  const painted =
    view.kind === "body" || view.kind === "body_with_retry" || view.kind === "body_stale"
      ? view.detail
      : null;

  const [editingField, setEditingField] = useState<EditingField>(null);
  // Snap back to read mode whenever the pane opens onto a different task (or
  // closes) so a stale inline draft never bleeds across tasks. Keys on the ID,
  // NOT the live detail — a task_changed refetch of the SAME task must not reset
  // an open editor and wipe the unsubmitted draft.
  useEffect(() => {
    setEditingField(null);
  }, [taskId]);
  // Mirror the unsettled-edit signal up to the host's request-close owner. A
  // ref read at click/keypress time, so `null → non-null` doesn't re-render App;
  // the cleanup clears it on unmount so a torn-down pane never leaves the gate
  // stuck closed.
  useEffect(() => {
    onEditingChange(editingField !== null);
  }, [editingField, onEditingChange]);
  useEffect(() => () => onEditingChange(false), [onEditingChange]);

  // Hidden while editing: navigating away would unmount the editor before its
  // blur-save settles.
  const jumpTarget =
    onJump && painted && editingField === null ? resolveTaskJumpTarget(painted) : null;
  const jumpAction = jumpTarget ? (
    <button
      type="button"
      data-task-view-jump
      data-sticker
      data-sticker-size="md"
      data-sticker-tone="neutral"
      title={t("tasks.viewInConversation")}
      aria-label={t("tasks.viewInConversation")}
      onClick={() => onJump?.(jumpTarget)}
      className="flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary transition-colors hover:bg-bg-hover hover:text-text-primary focus-outline"
    >
      <OpenExternalIcon className="size-3.5" aria-hidden />
    </button>
  ) : undefined;

  return (
    // `.task-pane` names the container query the metadata grid reads. `w-full`
    // makes it fill the SplitPane's right wrapper — without it the pane is a flex
    // row child that shrinks to its intrinsic content width (a thin strip); the
    // inner body's `flex-1` can only fill this root, not widen it (#task:1721).
    <div
      data-panel="side"
      className="task-pane flex h-full w-full flex-col overflow-hidden"
      style={paneChrome}
      aria-label={t("tasks.taskDetail")}
    >
      {/* The shared side-panel header, same primitive (and same title/subtitle
          idiom) the agent / thread / attachments panes use — so the close ✕ is
          the ONE `data-sticker-close` affordance poster stamps as a poster
          sticker, and the `h-12` height contract can't drift. Mirrors the
          attachments pane exactly: a static section label as the title, the
          entity's identity as the subtitle. */}
      <SidePanelHeader
        onClose={onClose}
        actions={jumpAction}
        nativeBar={{
          title: t("tasks.task"),
          subtitle: painted ? `#${painted.task_number}` : undefined,
          titleAction: null,
          actions: jumpTarget
            ? [
                [
                  {
                    id: "jump",
                    label: t("tasks.viewInConversation"),
                    icon: iconMarkup(OpenExternalIcon),
                  },
                ],
              ]
            : [],
          tabs: null,
          addTabs: null,
        }}
        nativeHandlers={{
          action: ({ id }) => {
            if (id === "jump" && jumpTarget) {
              onJump?.(jumpTarget);
            }
          },
        }}
      >
        <SidePanelHeaderTitle
          icon={<TaskIcon className="size-3.5 shrink-0 text-text-muted" aria-hidden />}
          title={t("tasks.task")}
          // The number, not the title — the task's own title lives in the body
          // with its edit pencil, so the header doesn't repeat it (shiki mockup
          // v2). Resolves with the read; blank until then, like the attachments
          // subtitle before its context loads.
          subtitle={painted ? `#${painted.task_number}` : ""}
        />
      </SidePanelHeader>
      <PaneContent
        view={view}
        editingField={editingField}
        onEditField={setEditingField}
        onStopEditing={() => setEditingField(null)}
        onRetry={() => void detailQuery.refetch()}
      />
    </div>
  );
}

/**
 * Total `switch` over the resolved view — the only place that decides what the
 * pane shows. Adding a state to `TaskDetailView` breaks this exhaustively
 * instead of falling into whichever branch happens to come first.
 */
function PaneContent({
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
      return <PaneStatePanel failure={null} />;
    case "not_found":
      return <PaneStatePanel failure="not_found" />;
    case "unreachable_empty":
      return <PaneStatePanel failure="unreachable" />;
    case "transient_empty":
      return <PaneStatePanel failure="transient" onRetry={onRetry} />;
    case "body":
    // `body_stale` is the SAME body: the connection is down, so the last read is
    // the best available truth and the one app-status banner already says the
    // connection is down. A second copy of that notice here would be a second
    // surface claiming one fact.
    case "body_stale":
      return (
        <PaneBody
          detail={view.detail}
          editingField={editingField}
          onEditField={onEditField}
          onStopEditing={onStopEditing}
        />
      );
    case "body_with_retry":
      return (
        <div className="flex min-h-0 flex-1 flex-col">
          {/* The refresh failed over a body we still hold. Nothing re-reads it on
              its own (`retry: 1`, no focus refetch), so the Retry has to be
              reachable here — but it stays a single muted line: the body below is
              still readable, this is a footnote about its freshness. */}
          <div className="flex flex-none items-center gap-3 border-b border-border-subtle px-4.5 py-2 text-xs text-text-muted">
            <span>{t("tasks.showingTheLastLoaded")}</span>
            <button
              type="button"
              onClick={onRetry}
              className="ml-auto shrink-0 rounded px-1 py-0.5 font-medium text-text-secondary underline-offset-2 hover:text-text-primary hover:underline focus-outline"
            >
              {t("common.retry")}
            </button>
          </div>
          <PaneBody
            detail={view.detail}
            editingField={editingField}
            onEditField={onEditField}
            onStopEditing={onStopEditing}
          />
        </div>
      );
  }
}

/** Loading + the three failure tiers, in plain markup (no Radix Dialog to
 *  satisfy). Copy is the shared `FAILURE_COPY`, so the two surfaces word a
 *  failure identically. */
function PaneStatePanel({
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
      <div
        role="status"
        aria-label={t("tasks.loadingTask")}
        className="min-h-0 flex-1 overflow-hidden px-4.5 py-4"
      >
        <div aria-hidden className="flex flex-col gap-4 motion-safe:animate-pulse">
          <div className="h-5 w-3/4 rounded-(--control-radius) bg-bg-elevated" />
          <div className="task-pane-meta items-center">
            {Array.from({ length: 6 }, (_, index) => (
              <Fragment key={index}>
                <div className="h-2.5 w-14 rounded-(--control-radius) bg-bg-elevated" />
                <div className="h-3 w-24 max-w-full rounded-(--control-radius) bg-bg-elevated" />
              </Fragment>
            ))}
          </div>
          <div className="flex flex-col gap-2">
            <div className="mb-1 h-2.5 w-20 rounded-(--control-radius) bg-bg-elevated" />
            <div className="h-3 w-full rounded-(--control-radius) bg-bg-elevated" />
            <div className="h-3 w-full rounded-(--control-radius) bg-bg-elevated" />
            <div className="h-3 w-2/3 rounded-(--control-radius) bg-bg-elevated" />
          </div>
        </div>
      </div>
    );
  }
  const copy = failureCopy()[failure];
  return (
    <div className="flex flex-col gap-1.5 px-4.5 py-6">
      <p className="text-sm font-semibold leading-snug text-text-primary">{copy.header}</p>
      <p className="text-sm text-text-muted">{copy.body}</p>
      {/* Retry ONLY for the transient tier: the query client is `retry: 1` with
          no focus refetch, so nothing else re-reads this. `not_found` is terminal
          and `unreachable` recovers through the app-status banner. */}
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

function PaneBody({
  detail,
  editingField,
  onEditField,
  onStopEditing,
}: {
  /** The AUTHORITATIVE read — the body only mounts once it resolves, so there is
   *  no "paint the card's summary while the real values arrive" window. */
  detail: TaskDetailDto;
  editingField: EditingField;
  onEditField: (field: Exclude<EditingField, null>) => void;
  onStopEditing: () => void;
}): JSX.Element {
  // Title/content, status and assignee share ONE local write slot: while any is
  // open or in flight the others go inert (see `useTaskWriteSlot`).
  const slot = useTaskWriteSlot(detail.id, editingField);
  const { busy } = slot;
  // Same field SSOT the modal sidebar renders — minus the Task# row, which this
  // surface's header already carries (chrome choice, not a field-set fork).
  const rows = useTaskMetaRows({ view: detail, ...slot }).filter((row) => row.key !== "task");

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto px-4.5 py-4">
      {editingField === "title" ? (
        <InlineFieldEditor
          taskId={detail.id}
          field="title"
          initial={detail.title}
          onDone={onStopEditing}
          onCancel={onStopEditing}
        />
      ) : (
        <div className="flex items-start gap-2">
          <h1 className="flex-1 text-base font-semibold leading-snug text-text-primary wrap-anywhere">
            {detail.title}
          </h1>
          {!busy ? (
            <MiniIconButton
              icon={EditIcon}
              label={t("tasks.editTitle")}
              className="mt-0.5"
              onClick={() => onEditField("title")}
            />
          ) : null}
        </div>
      )}

      {/* Metadata — width-adaptive columns via the `.task-pane-meta` container
          query. Label + value are FLAT grid children so a wide pane can pack two
          pairs per row. No box treatment: the parent's `gap-4` rhythm spaces it
          from title and description, so it reads flush instead of fenced. */}
      <div className="task-pane-meta items-start">
        {rows.map((row) => (
          <Fragment key={row.key}>
            <span className="chrome-label text-(length:--text-badge) font-medium leading-4 text-text-muted">
              {row.label}
            </span>
            <div className="min-w-0 text-xs leading-4 text-text-primary">
              {row.value}
              {row.error ? (
                <p className="mt-0.5 text-(length:--text-meta) text-error">{row.error}</p>
              ) : null}
            </div>
          </Fragment>
        ))}
      </div>

      <TaskHistorySection taskId={detail.id} />

      <section className="flex min-w-0 flex-col gap-2">
        <div className="flex items-center gap-2">
          <h2 className="chrome-label text-(length:--text-badge) font-medium text-text-muted">
            {t("tasks.description")}
          </h2>
          {!busy && editingField !== "content" ? (
            <MiniIconButton
              icon={EditIcon}
              label={t("tasks.editDescription")}
              onClick={() => onEditField("content")}
            />
          ) : null}
        </div>
        {editingField === "content" ? (
          <InlineFieldEditor
            taskId={detail.id}
            field="content"
            initial={detail.content}
            onDone={onStopEditing}
            onCancel={onStopEditing}
          />
        ) : detail.content.trim() ? (
          <Markdown content={detail.content} taskReferences="raw" />
        ) : (
          <p className="text-sm italic text-text-muted">{t("tasks.noDescription")}</p>
        )}
      </section>
    </div>
  );
}
