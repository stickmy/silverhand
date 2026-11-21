import type { MeResponse, RegistrationRequired, UserDto } from "@silverhand/shared";
import type { SetupRequired, ViewerState } from "./hooks";

/** Type guard for the setup-required auth state. Lives here, free of React
 *  Query, so pure modules can use it; `./hooks` re-exports it. */
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

/**
 * Renderer-side gate for editing a user's (human's) profile.
 *
 * Mirrors the v1 `/api/me` contract: only the viewer themselves can
 * edit their own profile. There is no admin / "edit-other-users"
 * surface in v1 — `PATCH /api/me` and `POST/DELETE /api/me/avatar` all
 * key off `requireViewerUserId(c)`, so the backend will reject any
 * cross-user attempt with 403 anyway. This helper lets the UI hide
 * those affordances ahead of the network round-trip.
 *
 * Returns false in three cases:
 *   1. `viewer === undefined` — the viewer query is still loading or
 *      errored. Render the form as read-only until we know.
 *   2. `viewer` is the `RegistrationRequired` shape — pre-registration
 *      should never reach a profile detail view, but if it does we
 *      keep the affordances hidden.
 *   3. The viewer's id does not match the target user id — different
 *      human, no edit. v1 single-user installs never hit this branch
 *      because the only navigable human is the viewer; v2 multi-human
 *      relies on it.
 */
export function canEditUserProfile(
  viewer: MeResponse | undefined,
  user: Pick<UserDto, "id">,
): boolean {
  if (!viewer) return false;
  if (isRegistrationRequired(viewer)) return false;
  return viewer.id === user.id;
}
