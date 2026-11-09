import { useCallback, useEffect, useState } from "react";
import {
  getAttachmentBlobUrl,
  loadAttachmentBlobUrl,
  useAttachmentBlobToken,
} from "./attachment-blob-cache";

/**
 * One attachment image's load, per generation (attachment id + blob token).
 *
 * - `loading`: fetching bytes. `url` keeps the previous generation's picture on
 *   screen so an in-place update swaps without a blank frame.
 * - `shown`: bytes for this generation are displayed.
 * - `failed`: the fetch failed (`url` keeps a previous generation's picture, if
 *   any), or the image failed to decode twice.
 *
 * `attempt` bounds recovery: the first `<img>` error is taken as a URL the
 * shared cache evicted and revoked, and is refetched once; a second error means
 * the bytes themselves will not decode. An unbounded retry repainted a broken
 * image every frame and flickered the whole UI (#task:1581).
 */
export type AttachmentImageLoad =
  | { status: "loading"; url: string | null; attempt: 1 | 2 }
  | { status: "shown"; url: string; attempt: 1 | 2 }
  | { status: "failed"; cause: "fetch"; message: string; url: string | null }
  | { status: "failed"; cause: "decode" };

export type AttachmentImageEvent =
  | { type: "generation"; cachedUrl: string | undefined }
  | { type: "loaded"; url: string }
  | { type: "fetchFailed"; message: string }
  | { type: "imageError" };

export function initialAttachmentImageLoad(cachedUrl: string | undefined): AttachmentImageLoad {
  return cachedUrl
    ? { status: "shown", url: cachedUrl, attempt: 1 }
    : { status: "loading", url: null, attempt: 1 };
}

export function nextAttachmentImageLoad(
  state: AttachmentImageLoad,
  event: AttachmentImageEvent,
): AttachmentImageLoad {
  switch (event.type) {
    case "generation": {
      // A new generation re-allows load and retry, so an undecodable
      // intermediate revision never poisons a later fixed one.
      if (event.cachedUrl) {
        return { status: "shown", url: event.cachedUrl, attempt: 1 };
      }
      return { status: "loading", url: attachmentImageUrl(state), attempt: 1 };
    }
    case "loaded": {
      if (state.status !== "loading") {
        return state;
      }
      return { status: "shown", url: event.url, attempt: state.attempt };
    }
    case "fetchFailed": {
      if (state.status !== "loading") {
        return state;
      }
      return { status: "failed", cause: "fetch", message: event.message, url: state.url };
    }
    case "imageError": {
      if (state.status === "failed") {
        return state;
      }
      if (state.attempt === 2) {
        return { status: "failed", cause: "decode" };
      }
      return { status: "loading", url: null, attempt: 2 };
    }
  }
}

/** The picture to put on screen, if any. A decode failure has none: its bytes
 *  are the ones that would not render. */
export function attachmentImageUrl(state: AttachmentImageLoad): string | null {
  if (state.status === "failed") {
    return state.cause === "fetch" ? state.url : null;
  }
  return state.url;
}

/** Loads an attachment image through the shared blob cache. Pass `enabled:
 *  false` for a non-image attachment; nothing is fetched. */
export function useAttachmentImage(
  attachmentId: string,
  enabled: boolean,
): { load: AttachmentImageLoad; onImageError: () => void } {
  const token = useAttachmentBlobToken(attachmentId);
  const generation = `${attachmentId}|${token}`;
  const [current, setCurrent] = useState(() => ({
    generation,
    load: initialAttachmentImageLoad(enabled ? getAttachmentBlobUrl(attachmentId) : undefined),
  }));

  let load = current.load;
  if (current.generation !== generation) {
    load = nextAttachmentImageLoad(current.load, {
      type: "generation",
      cachedUrl: enabled ? getAttachmentBlobUrl(attachmentId) : undefined,
    });
    setCurrent({ generation, load });
  }

  const loading = enabled && load.status === "loading";
  const attempt = load.status === "loading" ? load.attempt : 0;

  useEffect(() => {
    if (!loading) {
      return;
    }
    let cancelled = false;
    const send = (event: AttachmentImageEvent): void => {
      if (cancelled) {
        return;
      }
      setCurrent((prev) =>
        prev.generation === generation
          ? { generation, load: nextAttachmentImageLoad(prev.load, event) }
          : prev,
      );
    };
    loadAttachmentBlobUrl(attachmentId)
      .then((url) => send({ type: "loaded", url }))
      .catch((err: unknown) =>
        send({ type: "fetchFailed", message: err instanceof Error ? err.message : String(err) }),
      );
    return () => {
      cancelled = true;
    };
  }, [attachmentId, generation, loading, attempt]);

  const onImageError = useCallback(() => {
    setCurrent((prev) =>
      prev.generation === generation
        ? { generation, load: nextAttachmentImageLoad(prev.load, { type: "imageError" }) }
        : prev,
    );
  }, [generation]);

  return { load, onImageError };
}
