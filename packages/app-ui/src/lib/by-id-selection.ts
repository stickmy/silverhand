import { ApiError } from "@/lib/api";

/**
 * What a by-id read means for a selection outside the viewer's own list.
 * Only a definite 404 lets the page fall back to its default selection; any
 * other failure (server error, offline, unparseable body) keeps the target so
 * the page shows the error and can retry instead of silently moving away.
 */
export type ByIdSelection = "pending" | "kept" | "missing";

export function byIdSelection(read: { isSuccess: boolean; error: unknown }): ByIdSelection {
  if (read.isSuccess) {
    return "kept";
  }

  if (read.error === null || read.error === undefined) {
    return "pending";
  }

  return isNotFound(read.error) ? "missing" : "kept";
}

export function isNotFound(error: unknown): boolean {
  return error instanceof ApiError && error.kind === "http" && error.status === 404;
}
