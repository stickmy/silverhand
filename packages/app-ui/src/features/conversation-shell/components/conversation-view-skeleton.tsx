import { paneChrome } from "@/components/layout/pane-chrome";
import { SkeletonBlock } from "@/components/layout/skeleton";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useNativeBar, type PageBarDeclaration } from "@/components/layout/native-page-bar";
import { t } from "@/lib/i18n";

const LOADING_BAR: PageBarDeclaration = {
  title: "",
  titleAction: null,
  actions: [],
  tabs: null,
  addTabs: null,
};

// Stable examples, not inferred message data. Continuations preserve the avatar
// column but omit repeated identity, as MessageItem does for grouped messages.
const ROWS = [
  { author: "w-24", lines: ["w-4/5", "w-3/5"], continuation: false },
  { author: "w-24", lines: ["w-2/5"], continuation: true },
  { author: "w-16", lines: ["w-3/4"], continuation: false },
  { author: "w-28", lines: ["w-11/12", "w-4/5", "w-1/2"], continuation: false },
  { author: "w-28", lines: ["w-2/3", "w-1/3"], continuation: true },
  { author: "w-20", lines: ["w-1/2"], continuation: false },
  { author: "w-24", lines: ["w-4/5", "w-3/5"], continuation: false },
] as const;

/** MessageItem geometry, including its transparent frame and list's reading
 * width. Pure presentation: no queries, random widths or virtual-list effects. */
export function MessageRowsSkeleton({
  className = "min-h-0 flex-1",
}: {
  className?: string;
} = {}): JSX.Element {
  return (
    <div
      role="status"
      aria-label={t("conversationShell.loadingMessages")}
      className={`overflow-hidden ${className}`}
      data-conversation-rows-skeleton
    >
      <div aria-hidden className="mx-auto max-w-300 px-4 py-2 motion-safe:animate-pulse">
        {ROWS.map((row, index) => (
          <div key={index} className="py-0.5">
            <div
              className={`flex items-start gap-3 border-2 border-transparent px-2 ${row.continuation ? "py-0.5" : "py-1.5"}`}
            >
              <div className="mt-0.5 shrink-0" style={{ width: AVATAR_PX.lg }}>
                {!row.continuation ? (
                  <div style={{ height: AVATAR_PX.lg }}>
                    <SkeletonBlock className="size-full rounded-(--avatar-radius)" />
                  </div>
                ) : null}
              </div>
              <div className="min-w-0 w-full max-w-[85%]">
                {!row.continuation ? (
                  <div className="mb-1 flex h-4 items-center gap-2">
                    <SkeletonBlock className={`h-2.5 ${row.author}`} />
                    <SkeletonBlock className="h-2 w-10" />
                  </div>
                ) : null}
                {row.lines.map((width, line) => (
                  <div
                    key={line}
                    className="flex items-center"
                    style={{
                      height:
                        "calc(var(--appearance-markdown-font-size, var(--md-prose-size, 14px)) + 8px)",
                    }}
                  >
                    <div
                      className={`${width} rounded bg-bg-elevated`}
                      style={{
                        height: "var(--appearance-markdown-font-size, var(--md-prose-size, 14px))",
                      }}
                    />
                  </div>
                ))}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

/** Deferred chat mount placeholder; chrome stays separate from the message
 * loading state used by already-mounted channels and threads. */
export function ConversationViewSkeleton({
  showTabs = false,
}: {
  showTabs?: boolean;
}): JSX.Element {
  // The native bar would otherwise fall back to the section name ("Chats") and
  // read as the list, not the conversation that is loading.
  useNativeBar(LOADING_BAR);

  return (
    <main
      data-panel="chat"
      className="flex h-full min-w-0 flex-1 flex-col overflow-hidden"
      style={paneChrome}
      data-conversation-skeleton
    >
      <header
        data-conversation-header
        aria-hidden
        className="flex h-12 min-w-0 shrink-0 items-center gap-2 border-b border-border-subtle px-4"
      >
        <SkeletonBlock className="size-4" />
        <SkeletonBlock className="h-3.5 w-36 max-w-[50%]" />
        <div className="ml-auto flex gap-2">
          {[0, 1, 2].map((i) => (
            <SkeletonBlock key={i} className="size-4" />
          ))}
        </div>
      </header>
      {showTabs ? (
        <div
          aria-hidden
          className="flex h-(--conversation-tab-strip-height) shrink-0 items-center gap-4 border-b border-border-subtle px-2"
        >
          <SkeletonBlock className="h-3 w-12" />
          <SkeletonBlock className="h-3 w-12" />
        </div>
      ) : null}
      <MessageRowsSkeleton />
      <div
        aria-hidden
        className="shrink-0 border-t border-border-subtle bg-(--pane-bg,var(--bg-base)) px-3 py-3"
      >
        <div className="overflow-hidden rounded-(--control-radius) border border-border-subtle bg-bg-floating">
          <div className="min-h-15 px-3 py-2">
            <SkeletonBlock className="h-3 w-32" />
          </div>
          <div className="flex items-center justify-between px-2 py-1.5">
            <div className="flex gap-2">
              <SkeletonBlock className="size-4" />
              <SkeletonBlock className="size-4" />
            </div>
            <div className="h-7 w-7" />
          </div>
        </div>
      </div>
    </main>
  );
}
