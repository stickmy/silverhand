import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { Lightbox } from "./lightbox";
import { t } from "@/lib/i18n";

const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  share: vi.fn(),
  error: vi.fn(),
}));
vi.mock("react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("react")>()),
  useState: (value: unknown) => [value, vi.fn()],
  useRef: (value: unknown) => ({ current: value }),
  useCallback: (fn: unknown) => fn,
  useEffect: (fn: () => void | (() => void)) => {
    h.effects.push(fn);
  },
  useLayoutEffect: () => {},
}));
vi.mock("@/platform", () => ({
  getPlatform: () => ({ files: { sharePngExport: h.share } }),
}));
vi.mock("@/lib/pinch-zoom-lease", () => ({ usePinchZoomLease: () => {} }));
vi.mock("@/lib/toast", () => ({ toast: { error: h.error, success: vi.fn() } }));

function exportAction(loadExportBytes?: () => Promise<Uint8Array>) {
  const tree = Lightbox({
    open: true,
    onOpenChange: vi.fn(),
    src: "blob:expired-preview",
    alt: "attachment",
    loadExportBytes,
  });
  function find(node: unknown): (() => Promise<void>) | undefined {
    if (!node || typeof node !== "object") return;
    if (Array.isArray(node)) return node.map(find).find(Boolean);
    const props = (node as { props?: Record<string, unknown> }).props;
    if (!props) return;
    if (props.label === "Share") return props.onClick as () => Promise<void>;
    return find(props.children);
  }
  const cleanup = h.effects.map((effect) => effect());
  return { run: find(tree)!, unmount: () => cleanup.forEach((fn) => fn?.()) };
}

beforeEach(() => {
  h.effects = [];
  h.share.mockReset().mockResolvedValue({ ok: true });
  h.error.mockReset();
  vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("Load failed")));
});
afterEach(() => vi.unstubAllGlobals());

it("shares exact attachment bytes even when its display URL cannot be fetched", async () => {
  const bytes = new Uint8Array([0, 255, 1, 128]);
  const load = vi.fn().mockResolvedValue(bytes);
  await exportAction(load).run();
  expect(load).toHaveBeenCalledOnce();
  expect(fetch).not.toHaveBeenCalled();
  expect(h.share).toHaveBeenCalledExactlyOnceWith({ pngBytes: bytes });
});

it("does not fall back to cached display content when authenticated reading fails", async () => {
  await exportAction(async () => {
    throw new Error("Forbidden");
  }).run();
  expect(h.share).not.toHaveBeenCalled();
  expect(fetch).not.toHaveBeenCalled();
  expect(h.error).toHaveBeenCalledWith(t("messages.shareFailed", { reason: "Forbidden" }));
});

it("does not open native sharing when attachment reading completes after unmount", async () => {
  let resolve!: (bytes: Uint8Array) => void;
  const action = exportAction(
    () =>
      new Promise((done) => {
        resolve = done;
      }),
  );
  const pending = action.run();
  action.unmount();
  resolve(new Uint8Array([1]));
  await pending;
  expect(h.share).not.toHaveBeenCalled();
});

it("retains URL reading for local composer previews without an attachment reader", async () => {
  const bytes = new Uint8Array([10, 20]);
  vi.mocked(fetch).mockResolvedValue(new Response(bytes));
  await exportAction().run();
  expect(fetch).toHaveBeenCalledExactlyOnceWith("blob:expired-preview");
  expect(h.share).toHaveBeenCalledWith({ pngBytes: bytes });
});
