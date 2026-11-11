import type { ClientAuthState, ClientAuthResult } from "@silverhand/client";
import { getPlatform } from "@/platform";
import { agentStreamCache } from "@/features/agents/model/agent-stream-cache";
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import {
  AVATAR_DESIGN_FIELD,
  apiPaths,
  type AuthLoginInput,
  type AuthSignupInput,
  type MeResponse,
  type RegistrationRequired,
  type UpdateUserProfileInput,
  type UserDto,
} from "@silverhand/shared";
import { ApiError, api, apiUpload } from "@/lib/api";
import { clearCurrentUser, setCurrentUserId } from "@/features/session/current-user";
import { eventWsClient } from "@/lib/ws-client";
import { clearPlaceMemory } from "@/features/app-shell/place-memory";
import { avatarDesignKey } from "@/features/avatar-maker/hooks/use-avatar-design";
import type { DraftAvatar } from "@/features/avatar-maker/hooks/use-draft-avatar";
import {
  clearAvatarBlobCache,
  getAvatarBlobUrl,
  loadAvatarBlobUrl,
} from "@/features/identity/avatar-blob-cache";
import { CONVERSATION_MEMBERS_KEY_PREFIX } from "@/features/conversations/use-conversation-members";
import { clearSettingsSection } from "@/features/settings/model/settings-section";
import { clearTaskFilters } from "@/features/tasks/model/task-filters-memory";
import { usersKey } from "@/features/users/hooks";
import { serverEnvironmentKey } from "@/features/session/server-environment";

function clearViewerLocalState(): void {
  agentStreamCache.clear();
  clearPlaceMemory();
  clearAvatarBlobCache();
  clearSettingsSection();
  clearTaskFilters();
}

const meKey = ["me"] as const;

export type SetupRequired = RegistrationRequired;

export interface BackendUnreachable {
  backend_unreachable: true;
  server_url: string;
  message: string;
}

export interface UpdateRequired {
  update_required: true;
  direction: "app" | "server";
  server_api_epoch: number;
}

export type ViewerState = UserDto | SetupRequired | BackendUnreachable | UpdateRequired;

/**
 * Seat a fresh viewer dto everywhere the renderer keeps a copy of it.
 *
 * `['me']` is the viewer's own record, but it is NOT the only cache that
 * carries the viewer's `display_name` / `avatar_url`: the backend denormalizes
 * both into the global user directory (`['users']`) and into every
 * conversation roster (`['conversation-members', id]`, derived from the users
 * table on each read — see `membership-service.listMembers`). Writing only
 * `['me']` is what made a removed avatar linger in the channel members modal
 * and the header member strip: their DTOs still carried the old
 * `${path}?v=${updated_at}` url, and the session avatar-blob cache happily
 * served the bytes for it (#task:1701, shiki dogfood).
 *
 * Invalidate rather than patch: the identity fields are derived server-side
 * (the `?v=` cache-buster is a function of `updated_at`), so re-reading is the
 * only way to stay honest — hand-patching cached rosters would fork that
 * derivation into the renderer. Both keys are cheap directory reads and the
 * rosters not currently mounted just refetch on next mount.
 *
 * Exported for `viewer-identity-cache.test.ts`: "a viewer profile write
 * reseats every denormalized copy" is the contract every mutation below
 * delegates here for, so the test pins it once at the shared seam instead of
 * per-mutation.
 */
export function cacheViewer(qc: QueryClient, dto: UserDto): void {
  setCurrentUserId(dto.id, dto.display_name);
  qc.setQueryData(meKey, dto);
  void qc.invalidateQueries({ queryKey: usersKey });
  void qc.invalidateQueries({ queryKey: CONVERSATION_MEMBERS_KEY_PREFIX });
}

/**
 * Re-read the viewer's own record from the BACKEND and reseat every copy of it.
 *
 * The one identity cache that {@link invalidateIdentityCarrierCaches} cannot
 * converge: `['me']` is IPC-backed (`getAuthState()`), and main answers from the
 * in-memory session snapshot it took when the token was validated — so
 * invalidating the key just replays the same stale dto. Only a real
 * `GET /api/me` is authoritative, and going through the backend proxy reseats
 * main's snapshot on the way through (`main/profile-session-sync.ts`), which is
 * what keeps the NEXT `auth:getState` honest too.
 *
 * Local profile writes already reseat both sides via {@link cacheViewer} +
 * that proxy arm (#task:1701). This exists for the remote case: another machine
 * edited this viewer's profile and the `user_changed` frame is the only notice
 * this client gets (#task:1702).
 *
 * A failed re-read is deliberately silent — the previous snapshot stays, and
 * the next signal (or a remount) retries. Losing a name refresh is not worth
 * surfacing an error over a background sync.
 */
export async function reseatViewerFromBackend(qc: QueryClient): Promise<void> {
  try {
    const body = await api<MeResponse>(apiPaths.me);
    if (isRegistrationRequired(body)) return;
    setCurrentUserId(body.id, body.display_name);
    qc.setQueryData(meKey, body);
  } catch {
    // Offline / server down / token rejected — leave the cached viewer alone.
  }
}

/** Type guard for the setup-required auth state. */
export function isRegistrationRequired(body: ViewerState): body is SetupRequired;
export function isRegistrationRequired(body: MeResponse): body is RegistrationRequired;
export function isRegistrationRequired(
  body: MeResponse | ViewerState,
): body is RegistrationRequired | SetupRequired {
  return (
    typeof body === "object" &&
    body !== null &&
    "registration_required" in body &&
    (body as { registration_required: unknown }).registration_required === true
  );
}

export function isBackendUnreachable(body: ViewerState): body is BackendUnreachable {
  return (
    typeof body === "object" &&
    body !== null &&
    "backend_unreachable" in body &&
    (body as { backend_unreachable: unknown }).backend_unreachable === true
  );
}

export function isUpdateRequired(body: ViewerState): body is UpdateRequired {
  return (
    typeof body === "object" &&
    body !== null &&
    "update_required" in body &&
    (body as { update_required: unknown }).update_required === true
  );
}

export function getHydratedViewer(qc: QueryClient): UserDto | null {
  const state = qc.getQueryData<ViewerState>(meKey);
  if (!state) return null;
  if (isRegistrationRequired(state) || isBackendUnreachable(state) || isUpdateRequired(state)) {
    return null;
  }
  return state;
}

/**
 * pure mapping from the main-process IPC arm to
 * the renderer's `ViewerState` union. Extracted out of `useViewerUser`
 * so the three-way IPC → renderer mapping is testable without
 * React-DOM + React Query plumbing.
 *
 * The mapping is intentionally narrow:
 *   - `authenticated`        → `UserDto` (the `state.user` object)
 *   - `backend_unreachable`  → `BackendUnreachable`
 *   - `unauthenticated`      → `SetupRequired`
 *
 * Server features are not part of the session; they come from
 * `useServerEnvironment`.
 */
export function mapAuthStateToViewer(state: ClientAuthState): ViewerState {
  if (state.status === "authenticated") return state.user;
  if (state.status === "backend_unreachable") {
    return {
      backend_unreachable: true,
      server_url: state.server_url,
      message: state.message,
    };
  }
  // #task:1761 — epoch hard-gate arms map to the UpdateRequired marker.
  if (state.status === "app_update_required") {
    return {
      update_required: true,
      direction: "app",
      server_api_epoch: state.server_api_epoch,
    };
  }
  if (state.status === "server_update_required") {
    return {
      update_required: true,
      direction: "server",
      server_api_epoch: state.server_api_epoch,
    };
  }
  return { registration_required: true, suggested_name: "" };
}

/** A signed-in viewer survives losing the server: the read fails instead, so the
 *  app keeps its content and the status banner reports the connection. */
export async function readViewer(previous: ViewerState | undefined): Promise<ViewerState> {
  const state = await getPlatform().client.getAuthState();
  if (state.status === "backend_unreachable" && previous !== undefined && "id" in previous) {
    throw new ApiError(0, "Network Error", state.message, "backend_unreachable");
  }
  return mapAuthStateToViewer(state);
}

export function useViewerUser() {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: meKey,
    queryFn: () => readViewer(qc.getQueryData<ViewerState>(meKey)),
    staleTime: 30 * 1000,
  });
  useEffect(() => {
    if (
      query.data &&
      !isRegistrationRequired(query.data) &&
      !isBackendUnreachable(query.data) &&
      !isUpdateRequired(query.data)
    ) {
      setCurrentUserId(query.data.id, query.data.display_name);
    }
  }, [query.data]);
  return query;
}

export function subscribeRuntimeEpochPromotion(
  qc: QueryClient,
  subscribe: (handler: () => void) => () => void,
): () => void {
  return subscribe(() => {
    void qc.invalidateQueries({ queryKey: meKey });
  });
}

export function useRuntimeEpochPromotion(): void {
  const qc = useQueryClient();
  useEffect(
    () =>
      subscribeRuntimeEpochPromotion(qc, (handler) =>
        getPlatform().client.onEpochUpdateRequired(handler),
      ),
    [qc],
  );
}

export function authResultToUser(result: ClientAuthResult): UserDto {
  if (result.ok) return result.user;
  // preserve the typed backend-unreachable signal end-to-end.
  // `status: 0` is the sentinel main-process proxy uses to report that the
  // signup/login fetch never reached the server; we tag the
  // ApiError so callers branching on `err.kind` get the typed
  // arm instead of a generic "0 Network Error" HTTP error.
  const kind: "http" | "backend_unreachable" = result.status === 0 ? "backend_unreachable" : "http";
  throw new ApiError(result.status, result.statusText, result.bodyText, kind);
}

export function useSignupUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: AuthSignupInput) =>
      authResultToUser(await getPlatform().client.signup(input)),
    onSuccess: (dto) => {
      cacheViewer(qc, dto);
    },
  });
}

export function useLoginUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (input: AuthLoginInput) =>
      authResultToUser(await getPlatform().client.login(input)),
    onSuccess: (dto) => {
      cacheViewer(qc, dto);
    },
  });
}

/** Clear identity-owned state without detaching the mounted auth query observers. */
export function clearLoggedOutSession(qc: QueryClient): Promise<void> {
  clearCurrentUser();
  clearViewerLocalState();
  eventWsClient.disconnect();

  // clear() removes the observed me query without publishing a new result.
  // Keep that query attached so App leaves the shell and reads signed-out policy.
  // The server environment describes the server, not the signed-out account.
  qc.removeQueries({
    predicate: (query) =>
      query.queryKey[0] !== meKey[0] && query.queryKey[0] !== serverEnvironmentKey[0],
  });
  qc.getMutationCache().clear();
  return qc.resetQueries({ queryKey: meKey, exact: true });
}

export function useLogout() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => getPlatform().client.logout(),
    onSuccess: () => clearLoggedOutSession(qc),
  });
}

/** PATCH /api/me. By design only `display_name` is accepted —
 *  `name` is set once at registration. On success the cached viewer
 *  is invalidated so the next render picks up the new display name. */
export function useUpdateProfile() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateUserProfileInput) =>
      api<UserDto>(apiPaths.me, {
        method: "PATCH",
        body: JSON.stringify(input),
      }),
    onSuccess: (dto) => {
      cacheViewer(qc, dto);
    },
  });
}

export interface UploadAvatarVars {
  /** Renderer-side limit so users get fast feedback before the
   *  upload starts. Backend mirrors this with the same shared cap. */
  file: File;
  /** `avatarDesignField` output when the image came from the avatar maker. */
  design?: string;
}

/** POST /api/me/avatar (multipart). Returns the new avatar_url +
 *  full updated user dto. */
export function useUploadAvatar() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ file, design }: UploadAvatarVars) => {
      const form = new FormData();
      if (design) {
        form.set(AVATAR_DESIGN_FIELD, design);
      }
      form.set("file", file);
      return apiUpload<{ avatar_url: string; user: UserDto }>(apiPaths.meAvatar, form);
    },
    onSuccess: (result) => {
      cacheViewer(qc, result.user);
      qc.removeQueries({ queryKey: avatarDesignKey("me") });
    },
  });
}

/** Uploads the avatar chosen on the signup form once the account exists. Best effort:
 *  on failure the account keeps the initials fallback and the maker stays one click away. */
export function useUploadDraftViewerAvatar() {
  const qc = useQueryClient();

  return (draft: DraftAvatar) => {
    void (async () => {
      const upload = await draft.resolve();

      if (!upload) {
        return;
      }

      const form = new FormData();
      if (upload.design) {
        form.set(AVATAR_DESIGN_FIELD, upload.design);
      }
      form.set("file", upload.file);
      const result = await apiUpload<{ user: UserDto }>(apiPaths.meAvatar, form);
      cacheViewer(qc, result.user);
    })().catch(() => {});
  };
}

/**
 * Resolve the viewer's avatar to a renderable object URL.
 *
 * `UserDto.avatar_url` is a backend path that requires session auth.
 * `<img src>` can't carry headers, so we mirror the attachment
 * pattern: fetch the blob through the main-process auth proxy, wrap in
 * `URL.createObjectURL`, and revoke when the URL changes or the
 * consumer unmounts.
 *
 * Multiple consumers (SettingsPage + the active ChannelView, which
 * passes the URL down to every chat MessageItem) call this hook
 * concurrently. React Query's `useViewerUser` cache already de-dupes
 * the dto fetch; the blob fetch itself is small and the browser's HTTP
 * cache handles repeats. If profile loading is null/registration_required,
 * returns `null` so callers fall back to a placeholder.
 */
export function useViewerAvatarObjectUrl(): string | null {
  const { data } = useViewerUser();
  const avatarUrl =
    data && !isRegistrationRequired(data) && !isBackendUnreachable(data) && !isUpdateRequired(data)
      ? data.avatar_url
      : null;

  const [objectUrl, setObjectUrl] = useState<string | null>(() =>
    avatarUrl ? (getAvatarBlobUrl(avatarUrl) ?? null) : null,
  );
  useEffect(() => {
    if (!avatarUrl) {
      setObjectUrl(null);
      return;
    }
    const cached = getAvatarBlobUrl(avatarUrl);
    if (cached) {
      setObjectUrl(cached);
      return;
    }
    let cancelled = false;
    void loadAvatarBlobUrl(avatarUrl).then((url) => {
      if (!cancelled) setObjectUrl(url);
    });
    return () => {
      cancelled = true;
    };
  }, [avatarUrl]);
  return objectUrl;
}
