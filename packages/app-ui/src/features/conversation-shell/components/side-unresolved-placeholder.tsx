import { PaneGrip } from "@/components/layout/side-panel-header";
import { MessageRowsSkeleton } from "./conversation-view-skeleton";
import { CloseIcon } from "@/icons";
import { paneChrome } from "../../../components/layout/pane-chrome";
import { t } from "@/lib/i18n";

export type SidePlaceholderStatus = "loading" | "unauthorized" | "not_found" | "unresolved";

interface SidePlaceholderProps {
  /** The conversation pane's id. Shown verbatim (truncated) so the user can
   *  correlate the reference with whatever they clicked. */
  conversationId: string;
  /** Which non-resolved state the identity resolution reached; each gets its
   *  own copy so the user knows what to do. */
  status: SidePlaceholderStatus;
  /** The same close the resolved pane uses, so the X behaves identically. */
  onClose: () => void;
}

const statusCopy = (): Record<
  SidePlaceholderStatus,
  { header: string; body: string; hint?: string }
> => ({
  loading: {
    header: t("conversationShell.resolvingReference"),
    body: t("conversationShell.loadingThisConversation"),
  },
  unauthorized: {
    header: t("conversationShell.noAccess"),
    // No LEVEL is named: a 403 from `/ref` is not evidence of a missing
    // workspace. With a public-aware gate (#task:1692) most of what still
    // refuses is same-workspace — a private channel, an archived public one, a
    // DM the viewer isn't in — so "you don't have access to the workspace"
    // would be a false statement about the one thing the viewer can verify.
    body: t("conversationShell.youDontHaveAccess"),
    hint: t("conversationShell.itMayBePrivate"),
  },
  not_found: {
    header: t("conversationShell.conversationNotFound"),
    body: t("conversationShell.thisReferencePointsTo"),
    hint: t("conversationShell.closeTheReferenceOr"),
  },
  // Distinct from `not_found` on purpose (#task:2038): a private line can only be
  // named by the viewer's direct-pair list, so when THAT read fails nothing has
  // been proven about the reference. Claiming deletion here would be a false
  // statement, and a loader would never end — nothing is in flight.
  unresolved: {
    header: t("conversationShell.referenceUnavailable"),
    body: t("conversationShell.couldntResolveThisConversation"),
  },
});

/**
 * A conversation pane whose conversation hasn't resolved (loading /
 * unauthorized / not_found / unresolved). Never silently strip the URL: the
 * user sees that the reference can't load and has an explicit Close. State
 * only — the parent passes the status derived from the pane's descriptor.
 */
export function SideUnresolvedPlaceholder({
  conversationId,
  status,
  onClose,
}: SidePlaceholderProps): JSX.Element {
  // Truncate long ids so the placeholder doesn't overflow narrow panes.
  const shortId =
    conversationId.length > 20
      ? `${conversationId.slice(0, 8)}…${conversationId.slice(-4)}`
      : conversationId;
  const copy = statusCopy()[status];
  return (
    <main data-panel="side" className="flex h-full min-w-0 flex-1 flex-col" style={paneChrome}>
      <header
        data-conversation-header
        className="flex h-(--titlebar-height,44px) shrink-0 items-center justify-between border-b border-border px-3"
      >
        <PaneGrip />
        <div className="min-w-0 flex-1 truncate text-sm font-medium text-text-muted">
          {copy.header}
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label={t("conversationShell.closeSidePane")}
          title={t("conversationShell.closeSidePane")}
          className="ml-2 flex size-7 shrink-0 items-center justify-center rounded text-text-secondary hover:bg-bg-hover hover:text-text-primary"
        >
          <CloseIcon className="size-4" />
        </button>
      </header>
      {status === "loading" ? (
        <MessageRowsSkeleton />
      ) : (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center text-sm text-text-muted">
          <p>{copy.body}</p>
          <p className="font-mono text-xs text-text-faint">#conv:{shortId}</p>
          {copy.hint ? <p className="text-xs text-text-faint">{copy.hint}</p> : null}
        </div>
      )}
    </main>
  );
}
