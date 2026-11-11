import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useMemo,
  useLayoutEffect,
  type ReactNode,
} from "react";

export interface ReadingPosition {
  anchor?: string;
  top?: number;
  atBottom: boolean;
}
const ViewMemory = createContext<Map<string, ReadingPosition> | null>(null);

const STORAGE_KEY = "silverhand.view-memory";

/** Reading positions mirrored to sessionStorage. iOS may end the web content
 * process of a backgrounded app; the reload that follows keeps sessionStorage,
 * so the reader returns to where they were. A fresh launch starts empty. */
class SessionViewMemory extends Map<string, ReadingPosition> {
  constructor() {
    super(readStored());
  }

  override set(key: string, value: ReadingPosition): this {
    super.set(key, value);
    try {
      sessionStorage.setItem(STORAGE_KEY, JSON.stringify([...this]));
    } catch {
      // Storage full or unavailable: positions stay in memory only.
    }
    return this;
  }
}

function readStored(): [string, ReadingPosition][] {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as [string, ReadingPosition][]) : [];
  } catch {
    return [];
  }
}

/** Lives inside the authenticated boundary. Leaving it (sign-out) drops the
 * stored positions, so they never carry over to another viewer. */
export function ViewMemoryProvider({ children }: { children: ReactNode }) {
  const memory = useRef<SessionViewMemory | null>(null);
  memory.current ??= new SessionViewMemory();

  useEffect(() => {
    return () => {
      try {
        sessionStorage.removeItem(STORAGE_KEY);
      } catch {
        // Nothing stored.
      }
    };
  }, []);

  return <ViewMemory.Provider value={memory.current}>{children}</ViewMemory.Provider>;
}
export const useViewMemory = () => useContext(ViewMemory);

/** Restore ordinary lists after their cached rows have mounted. */
export function useListReadingPosition(key: string) {
  const memory = useViewMemory();
  const ref = useRef<HTMLElement | null>(null);
  const restore = useMemo(
    () => ({ pending: true, top: memory?.get(key)?.top ?? 0 }),
    [key, memory],
  );
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node || !restore.pending) return;
    if (restore.top > 0 && node.scrollHeight <= node.clientHeight) return;
    node.scrollTop = restore.top;
    restore.pending = false;
  });
  return {
    ref: (node: HTMLElement | null) => {
      ref.current = node;
    },
    onScroll: () => {
      if (ref.current && !restore.pending)
        memory?.set(key, { top: ref.current.scrollTop, atBottom: false });
    },
  };
}
