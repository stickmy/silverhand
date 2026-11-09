import { describe, expect, it, vi } from "vitest";
import { saveAttachmentToDisk, type SaveAttachmentDeps } from "./save-attachment";

function makeDeps(overrides: Partial<SaveAttachmentDeps> = {}): {
  deps: SaveAttachmentDeps;
  fetchBytes: ReturnType<typeof vi.fn>;
  save: ReturnType<typeof vi.fn>;
  toastSuccess: ReturnType<typeof vi.fn>;
  toastError: ReturnType<typeof vi.fn>;
} {
  const fetchBytes = vi.fn(async () => ({ bytes: new Uint8Array([1, 2, 3]) }) as const);
  const save = vi.fn(
    async () =>
      ({ ok: true, path: "/tmp/out.bin" }) as {
        ok: true;
        path: string;
      },
  );
  const toastSuccess = vi.fn();
  const toastError = vi.fn();
  const deps: SaveAttachmentDeps = {
    fetchBytes: overrides.fetchBytes ?? fetchBytes,
    save: overrides.save ?? save,
    toastSuccess: overrides.toastSuccess ?? toastSuccess,
    toastError: overrides.toastError ?? toastError,
  };
  return { deps, fetchBytes, save, toastSuccess, toastError };
}

describe("saveAttachmentToDisk", () => {
  it("success: bytes fetched, saved, success toast fires once", async () => {
    const { deps, fetchBytes, save, toastSuccess, toastError } = makeDeps();
    await saveAttachmentToDisk({ id: "att_1", filename: "a.txt" }, deps);
    expect(fetchBytes).toHaveBeenCalledWith("att_1");
    expect(save).toHaveBeenCalledWith({
      filename: "a.txt",
      bytes: new Uint8Array([1, 2, 3]),
    });
    expect(toastSuccess).toHaveBeenCalledTimes(1);
    expect(toastError).not.toHaveBeenCalled();
  });

  it("cancel: no toast fires — user dismissing the picker is not an error", async () => {
    // Falsifier for the "toast on every failure" bug — cancel is
    // structurally `{ ok: false, canceled: true }` and MUST be silent.
    const save = vi.fn(async () => ({ ok: false, canceled: true }) as const);
    const { deps, toastSuccess, toastError } = makeDeps({ save });
    await saveAttachmentToDisk({ id: "att_1", filename: "a.txt" }, deps);
    expect(toastSuccess).not.toHaveBeenCalled();
    expect(toastError).not.toHaveBeenCalled();
  });

  it("IPC failure: error toast surfaces the reason", async () => {
    const save = vi.fn(async () => ({
      ok: false as const,
      reason: "write_failed:EACCES",
    }));
    const { deps, toastError, toastSuccess } = makeDeps({ save });
    await saveAttachmentToDisk({ id: "att_1", filename: "a.txt" }, deps);
    expect(toastError).toHaveBeenCalledTimes(1);
    expect(toastError.mock.calls[0]?.[0]).toContain("write_failed:EACCES");
    expect(toastSuccess).not.toHaveBeenCalled();
  });

  it("fetch failure: error toast, save not called", async () => {
    const fetchBytes = vi.fn(async () => {
      throw new Error("network down");
    });
    const save = vi.fn();
    const { deps, toastError, toastSuccess } = makeDeps({
      fetchBytes,
      save,
    });
    await saveAttachmentToDisk({ id: "att_1", filename: "a.txt" }, deps);
    expect(save).not.toHaveBeenCalled();
    expect(toastError).toHaveBeenCalledTimes(1);
    expect(toastError.mock.calls[0]?.[0]).toContain("network down");
    expect(toastSuccess).not.toHaveBeenCalled();
  });

  it("non-Error rejection is stringified into the toast", async () => {
    const fetchBytes = vi.fn(async () => {
      // eslint-disable-next-line @typescript-eslint/no-throw-literal
      throw "raw string";
    });
    const { deps, toastError } = makeDeps({ fetchBytes });
    await saveAttachmentToDisk({ id: "att_1", filename: "a.txt" }, deps);
    expect(toastError.mock.calls[0]?.[0]).toContain("raw string");
  });
});

describe("saveAttachmentToDisk while a save is running", () => {
  it("a repeat call joins it instead of fetching again", async () => {
    let release: (bytes: { bytes: Uint8Array }) => void = () => {};
    const fetchBytes = vi.fn(
      () => new Promise<{ bytes: Uint8Array }>((resolve) => (release = resolve)),
    );
    const { deps, save } = makeDeps({ fetchBytes });
    const first = saveAttachmentToDisk({ id: "att_2", filename: "a.txt" }, deps);
    await saveAttachmentToDisk({ id: "att_2", filename: "a.txt" }, deps);
    release({ bytes: new Uint8Array([1]) });
    await first;
    expect(fetchBytes).toHaveBeenCalledTimes(1);
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("a failed save releases the attachment for the next try", async () => {
    const fetchBytes = vi
      .fn()
      .mockRejectedValueOnce(new Error("network down"))
      .mockResolvedValueOnce({ bytes: new Uint8Array([1]) });
    const { deps, save } = makeDeps({ fetchBytes });
    await saveAttachmentToDisk({ id: "att_3", filename: "a.txt" }, deps);
    await saveAttachmentToDisk({ id: "att_3", filename: "a.txt" }, deps);
    expect(fetchBytes).toHaveBeenCalledTimes(2);
    expect(save).toHaveBeenCalledTimes(1);
  });
});
