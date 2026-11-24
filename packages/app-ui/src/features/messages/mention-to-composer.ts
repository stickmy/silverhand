import { createContext, useContext } from "react";

/**
 * Clicking an author's name mentions them in the composer of the stream the
 * message is in. The list and the composer are sibling views, so they meet on
 * a window event keyed by the composer's `draftKey` (as Quote does). A stream
 * with no usable composer has no listener, and the click does nothing.
 */
const MENTION_EVENT = "silverhand:mention-to-composer";

interface MentionDetail {
  draftKey: string;
  handle: string;
}

/** The draft key of the composer a message list writes to. */
export const ComposerTarget = createContext<string | null>(null);

export function useComposerTarget(): string | null {
  return useContext(ComposerTarget);
}

export function requestMentionInComposer(draftKey: string, handle: string): void {
  window.dispatchEvent(
    new CustomEvent<MentionDetail>(MENTION_EVENT, { detail: { draftKey, handle } }),
  );
}

export function onMentionInComposer(
  draftKey: string,
  handler: (handle: string) => void,
): () => void {
  const listener = (event: Event): void => {
    const detail = (event as CustomEvent<MentionDetail>).detail;
    if (detail && detail.draftKey === draftKey) {
      handler(detail.handle);
    }
  };
  window.addEventListener(MENTION_EVENT, listener);
  return () => window.removeEventListener(MENTION_EVENT, listener);
}
