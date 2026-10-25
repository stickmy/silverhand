/**
 * Thin HTTP client to the Silverhand backend's canonical
 * `/internal/agents/:aid/...` route family.
 *
 * All failures funnel through `errors.mapHttpError` so the agent sees
 * stable codes instead of raw HTTP. Bearer auth uses the
 * `SILVERHAND_AUTH_TOKEN` value loaded from env; the complete runtime
 * principal and its live target policy are enforced server-side.
 */
import {
  AUTH_TOKEN_HEADER,
  internalPaths,
  type AgentListChannelsDto,
  type AgentCreateChannelInput,
  type AgentFollowThreadInput,
  type AgentFollowThreadOutput,
  type AgentListWorkspacesDto,
  type AgentProfileDto,
  type AgentReadHistoryDto,
  type AgentReadHistoryInput,
  type AgentSearchMessagesDto,
  type AgentSearchMessagesInput,
  type AgentSendStaleResolutionReason,
  type AttachmentDto,
  type AttachmentLocateDto,
  type ClaimTasksInput,
  type ClaimTasksResult,
  type ConversationMembersResponse,
  type CreateTasksInput,
  type CreateTasksResult,
  type EditTaskInput,
  type McpProbeResult,
  type Reference,
  type ScheduleCancelInput,
  type ScheduleCancelOutput,
  type ScheduleCreateInput,
  type ScheduleCreateOutput,
  type ScheduleListInput,
  type ScheduleListOutput,
  type TaskDetailDto,
  type TaskEditResult,
  type TaskMutationResult,
  type TaskStatus,
  type TaskSummaryDto,
} from '@silverhand/shared';
import { readFile } from 'node:fs/promises';
import { basename, isAbsolute } from 'node:path';
import { CliError, fromNetworkError, mapHttpError } from './errors.js';
import { markRequestAttempted } from './cli-trace.js';
import type { CliEnv } from './env.js';

export class BackendClient {
  constructor(private readonly env: CliEnv) {}

  private headers(): Record<string, string> {
    return {
      [AUTH_TOKEN_HEADER]: `Bearer ${this.env.authToken}`,
      'X-Agent-Hub-Caller': 'silverhand-cli',
      'Content-Type': 'application/json',
    };
  }

  private async request<T>(
    method: 'GET' | 'POST' | 'DELETE',
    path: string,
    body?: unknown,
    query?: Record<string, string | number | readonly string[] | readonly number[] | undefined>,
  ): Promise<T> {
    // Mark BEFORE the network call so any in-flight error is treated
    // as a backend-originated failure (already logged server-side as
    // `cli.invoke`) and the pre-flight tracer stays silent.
    markRequestAttempted();
    const qs = query ? buildQueryString(query) : '';
    const url = `${this.env.serverUrl}${path}${qs}`;
    let res: Response;
    try {
      res = await fetch(url, {
        method,
        headers: this.headers(),
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    return (await res.json()) as T;
  }

  // ── Messaging ────────────────────────────────────────────────────
  sendMessage(input: {
    content: string;
    /** The addressed stream, verbatim as the actor spelled it (#task:2021
     *  P4-4). Required, and a raw string on purpose: the grammar has one
     *  authority (`delivery-target.ts`) and the resolver is the one place that
     *  turns an address into a stream, so this client re-spells nothing.
     *
     *  The route in the path below is unrelated to it — that is the credential's
     *  conversation, which says who is sending, never where the words go. */
    target: string;
    stale_event_id?: string;
    stale_resolution_reason?: AgentSendStaleResolutionReason;
    freshness_bypass?: boolean;
    attachment_ids?: string[];
  }): Promise<{
    message_id: string;
    message_seq?: number;
    thread_root_id?: string | null;
    /** References in the body that addressed nobody (#task:2021 P4-5-b).
     *  Passed straight through from the backend response: this client validates
     *  nothing, and the field is absent unless the server said something. */
    unresolved_references?: readonly Reference[];
  }> {
    return this.request(
      'POST',
      internalPaths.agentSendConversationMessage(this.env.agentId),
      input,
    );
  }

  resolveStaleMessage(input: {
    stale_event_id: string;
    resolution_reason: 'obsolete';
  }): Promise<{ ok: boolean }> {
    return this.request('POST', internalPaths.agentResolveStaleMessage(this.env.agentId), input);
  }

  /** Add or remove the agent's own emoji reaction on a message
   *  (docs/cli-system.md §3.6). Idempotent: re-adding / re-removing
   *  returns `changed: false`. */
  reactToMessage(input: {
    message_id: string;
    emoji: string;
    remove?: boolean;
  }): Promise<{ changed: boolean; emoji: string; count: number }> {
    return this.request(
      input.remove ? 'DELETE' : 'POST',
      internalPaths.agentReactMessage(this.env.agentId),
      { message_id: input.message_id, emoji: input.emoji },
    );
  }

  /** Voluntarily follow a reply cluster (subscription, not permission;
   *  §5 B). `root_message_id` may be the cluster root or any reply in it;
   *  the server normalizes to the canonical root and backfills its prior
   *  history into this agent's inbox. */
  followThread(input: AgentFollowThreadInput): Promise<AgentFollowThreadOutput> {
    return this.request('POST', internalPaths.agentFollowThread(this.env.agentId), input);
  }

  /** Stop following a reply cluster (§5). Idempotent; a later mention /
   *  reply re-follows automatically. */
  unfollowThread(input: AgentFollowThreadInput): Promise<AgentFollowThreadOutput> {
    return this.request('POST', internalPaths.agentUnfollowThread(this.env.agentId), input);
  }

  createChannel(input: AgentCreateChannelInput): Promise<{ channel_id: string }> {
    return this.request('POST', internalPaths.agentCreateChannel(this.env.agentId), input);
  }

  /**
   * Add an existing workspace member (agent or human) to the current
   * conversation (channel or thread). Server-side handle resolution:
   * the route resolves the handle against the conversation's workspace
   * member set, so the CLI never needs to enumerate users / workspace
   * members.
   *
   * The internal route (`POST /internal/agents/:aid/members`)
   * accepts ONLY `{ handle }` — agents cannot supply `{kind,id}` because
   * resolving an id would require the agent to enumerate the workspace
   * roster first, which contradicts the bounded read surface invariant.
   */
  addConversationMember(input: {
    handle: string;
    /** WHICH conversation gains the member (#task:2021 P4-4). The path below is
     *  the credential's route and says who is asking, never where the roster
     *  change lands. Forwarded verbatim — including a `/thread:` suffix, which
     *  this door refuses server-side rather than the CLI deciding that a
     *  membership is or is not a cluster-level thing. */
    target: string;
  }): Promise<ConversationMembersResponse> {
    return this.request('POST', internalPaths.agentAddConversationMember(this.env.agentId), input);
  }

  readHistory(input: AgentReadHistoryInput): Promise<AgentReadHistoryDto> {
    return this.request('POST', internalPaths.agentReadHistory(this.env.agentId), input);
  }

  listWorkspaces(): Promise<AgentListWorkspacesDto> {
    return this.request('GET', internalPaths.agentListWorkspaces(this.env.agentId));
  }

  listChannels(input: { workspace_id?: string } = {}): Promise<AgentListChannelsDto> {
    return this.request('GET', internalPaths.agentListChannels(this.env.agentId), undefined, {
      workspace_id: input.workspace_id,
    });
  }

  searchMessages(input: AgentSearchMessagesInput): Promise<AgentSearchMessagesDto> {
    return this.request('POST', internalPaths.agentSearchMessages(this.env.agentId), input);
  }

  // ── Tasks ────────────────────────────────────────────────────────
  listTasks(filters: {
    workspace_id?: string;
    workspace_name?: string;
    target?: string;
    conversation_id?: string;
    /** Multi-value: passed as repeated query keys. */
    status?: readonly TaskStatus[];
    /** Multi-value: passed as repeated query keys. */
    task_number?: readonly number[];
    assignee_id?: string;
    limit?: number;
  }): Promise<{ tasks: TaskSummaryDto[] }> {
    return this.request(
      'GET',
      internalPaths.agentListConversationTasks(this.env.agentId),
      undefined,
      {
        workspace_id: filters.workspace_id,
        workspace_name: filters.workspace_name,
        target: filters.target,
        conversation_id: filters.conversation_id,
        status: filters.status,
        task_number: filters.task_number,
        assignee_id: filters.assignee_id,
        limit: filters.limit,
      },
    );
  }

  createTasks(input: CreateTasksInput): Promise<CreateTasksResult> {
    return this.request(
      'POST',
      internalPaths.agentCreateConversationTasks(this.env.agentId),
      input,
    );
  }

  claimTasks(input: ClaimTasksInput): Promise<ClaimTasksResult> {
    return this.request('POST', internalPaths.agentClaimConversationTasks(this.env.agentId), input);
  }

  unclaimTask(input: {
    task_number?: number;
    task_id?: string;
    workspace_id?: string;
    workspace_name?: string;
  }): Promise<TaskMutationResult> {
    return this.request(
      'POST',
      internalPaths.agentUnclaimConversationTask(this.env.agentId),
      input,
    );
  }

  updateTaskStatus(input: {
    task_number?: number;
    task_id?: string;
    status: TaskStatus;
    workspace_id?: string;
    workspace_name?: string;
  }): Promise<TaskMutationResult> {
    return this.request(
      'POST',
      internalPaths.agentUpdateConversationTaskStatus(this.env.agentId),
      input,
    );
  }

  /** Edit a task's title/content (last-write-wins). The route returns 200
   *  with the typed envelope, so the caller reads `success` / `code` from
   *  the result. */
  editTask(input: EditTaskInput): Promise<TaskEditResult> {
    return this.request('POST', internalPaths.agentEditConversationTask(this.env.agentId), input);
  }

  getTaskDetail(input: {
    task_number?: number;
    task_id?: string;
    workspace_id?: string;
    workspace_name?: string;
  }): Promise<{ task: TaskDetailDto }> {
    return this.request(
      'GET',
      internalPaths.agentGetConversationTaskDetail(this.env.agentId),
      undefined,
      {
        task_number: input.task_number,
        task_id: input.task_id,
        workspace_id: input.workspace_id,
        workspace_name: input.workspace_name,
      },
    );
  }

  // ── Schedules ────────────────────────────────────────────────────
  createSchedule(input: ScheduleCreateInput): Promise<ScheduleCreateOutput> {
    return this.request('POST', internalPaths.agentScheduleCreate(this.env.agentId), input);
  }

  listSchedules(input: ScheduleListInput): Promise<ScheduleListOutput> {
    return this.request('POST', internalPaths.agentScheduleList(this.env.agentId), input);
  }

  cancelSchedule(input: ScheduleCancelInput): Promise<ScheduleCancelOutput> {
    return this.request('POST', internalPaths.agentScheduleCancel(this.env.agentId), input);
  }

  // ── Profile (L1 agent-global config) ─────────────────────────────
  /** Read the agent's own profile MCP config (#task:1135). `raw` is the
   *  verbatim `mcp.json` text, or `null` when the agent has none. */
  getProfileMcp(): Promise<{ raw: string | null }> {
    return this.request('GET', internalPaths.agentProfileMcpGet(this.env.agentId));
  }

  /** Replace the agent's own profile MCP config (#task:1135). Sends the
   *  raw JSON text VERBATIM (not re-encoded through JSON.stringify) so the
   *  persisted file keeps the agent's own formatting; the backend reads it
   *  via `c.req.text()`, validates it, then writes it atomically on the
   *  owning host. Effect is next-spawn (no hot reload). */
  async setProfileMcp(raw: string): Promise<{ ok: boolean; bytes_written: number }> {
    markRequestAttempted();
    const url = this.env.serverUrl + internalPaths.agentProfileMcpSet(this.env.agentId);
    let res: Response;
    try {
      res = await fetch(url, {
        method: 'POST',
        headers: this.headers(),
        body: raw,
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    return (await res.json()) as { ok: boolean; bytes_written: number };
  }

  /** Live-probe one of the agent's own profile MCP servers (#task:1137).
   *  Opens a one-shot connection on the host (initialize + optional
   *  tools/list) and returns the diagnostic snapshot. `serverName` is the RAW
   *  name; connection/handshake/tools failures come back as
   *  `{ ok:false, phase, message }`, not thrown. */
  probeProfileMcp(serverName: string, includeTools: boolean): Promise<McpProbeResult> {
    return this.request('POST', internalPaths.agentProfileMcpProbe(this.env.agentId), {
      server_name: serverName,
      include_tools: includeTools,
    });
  }

  /** Read the agent's own profile metadata (#task:1178) — display_name /
   *  description / avatar_url + identity. Backs `silverhand profile get`. */
  getProfile(): Promise<{ profile: AgentProfileDto }> {
    return this.request('GET', internalPaths.agentProfileGet(this.env.agentId));
  }

  /** Read ANOTHER agent's profile by handle (#task:1823) — read-only, same
   *  `AgentProfileDto` as the self read. `workspaceId` selects the workspace
   *  the co-membership is evaluated in (omitted = the source conversation's).
   *  A target the caller cannot see is an ordinary `not_found`. Backs
   *  `silverhand profile get --handle <name>`. */
  getPeerProfile(handle: string, workspaceId?: string): Promise<{ profile: AgentProfileDto }> {
    const query = new URLSearchParams({ handle });
    if (workspaceId !== undefined) query.set('workspace_id', workspaceId);
    return this.request(
      'GET',
      `${internalPaths.agentGetPeerProfile(this.env.agentId)}?${query.toString()}`,
    );
  }

  /** Update the agent's own profile in ONE request (#task:1178). Text
   *  fields + the avatar action travel together as multipart/form-data so
   *  the whole edit is applied by a single backend handler (no partial
   *  write across calls) and the avatar bytes ride a `file` part (never
   *  base64 in JSON). The backend's avatar service is the authoritative
   *  MIME/size/storage validator. Returns the updated profile. */
  async updateProfile(input: {
    display_name?: string;
    description?: string;
    avatar?:
      | { action: 'set'; buffer: Buffer; filename: string; contentType: string }
      | { action: 'clear' };
  }): Promise<{ profile: AgentProfileDto }> {
    markRequestAttempted();
    const url = this.env.serverUrl + internalPaths.agentProfileUpdate(this.env.agentId);
    const form = new FormData();
    if (input.display_name !== undefined) form.append('display_name', input.display_name);
    if (input.description !== undefined) form.append('description', input.description);
    if (input.avatar?.action === 'set') {
      form.append('avatar_action', 'set');
      const blob = new Blob([input.avatar.buffer], { type: input.avatar.contentType });
      form.append('file', blob, input.avatar.filename);
    } else if (input.avatar?.action === 'clear') {
      form.append('avatar_action', 'clear');
    }
    let res: Response;
    try {
      res = await fetch(url, {
        method: 'POST',
        // No Content-Type — fetch sets the multipart boundary itself.
        headers: {
          [AUTH_TOKEN_HEADER]: `Bearer ${this.env.authToken}`,
          'X-Agent-Hub-Caller': 'silverhand-cli',
        },
        body: form,
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    return (await res.json()) as { profile: AgentProfileDto };
  }

  // ── Attachments ──────────────────────────────────────────────────
  /** Upload a file from disk to the conversation `target` names.
   *
   *  The address travels as a form FIELD beside the bytes rather than in the
   *  path: the path segments are the credential's `(cid, aid)` — where the
   *  token was minted — and the destination is now a separate fact
   *  (#task:2021 P4-4). The route reads the field before the bytes; Hono
   *  caches the parsed form so nothing is read twice. */
  async uploadAttachment(input: {
    path: string;
    target: string;
    filename?: string;
    contentType?: string | null;
  }): Promise<AttachmentDto> {
    if (!isAbsolute(input.path)) {
      throw new CliError({
        code: 'validation_error',
        message: 'file upload --path must be absolute',
        retriable: false,
      });
    }
    let buffer: Buffer;
    try {
      buffer = await readFile(input.path);
    } catch (err) {
      throw new CliError({
        code: 'not_found',
        message: `cannot read file at ${input.path}: ${
          err instanceof Error ? err.message : String(err)
        }`,
        retriable: false,
      });
    }
    const url = this.env.serverUrl + internalPaths.agentUploadAttachment(this.env.agentId);
    const form = new FormData();
    // The address first, verbatim: the CLI does not decompose or normalize it,
    // so the ONE grammar authority on the server side answers a malformed
    // token, and a header's `target=` string pastes straight through.
    form.append('target', input.target);
    // Important: when the agent did NOT pass `--content-type`, leave the
    // Blob's type empty rather than forcing `application/octet-stream`.
    // The backend treats both empty and `application/octet-stream` as
    // "no useful hint" and falls through to magic-bytes + suffix
    // detection — so an HTML file uploaded by an agent (which never
    // typed a MIME) still resolves to `text/html` and renders correctly.
    // See `packages/shared/src/mime.ts` for the shared contract.
    const blob = new Blob([buffer], {
      type: input.contentType ?? '',
    });
    form.append('file', blob, input.filename ?? basename(input.path));
    let res: Response;
    markRequestAttempted();
    try {
      res = await fetch(url, {
        method: 'POST',
        headers: {
          [AUTH_TOKEN_HEADER]: `Bearer ${this.env.authToken}`,
          'X-Agent-Hub-Caller': 'silverhand-cli',
        },
        body: form,
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    return (await res.json()) as AttachmentDto;
  }

  /** Replace the bytes of an attachment this agent uploaded, in place. The id,
   *  filename, and content-type are unchanged — only the content updates, so a
   *  reviewer's resolved local path (`file view`) stays valid across iterations.
   *  Backend enforces creator ownership + same-channel scope. */
  async updateAttachment(input: { attachmentId: string; path: string }): Promise<AttachmentDto> {
    if (!isAbsolute(input.path)) {
      throw new CliError({
        code: 'validation_error',
        message: 'file update --path must be absolute',
        retriable: false,
      });
    }
    let buffer: Buffer;
    try {
      buffer = await readFile(input.path);
    } catch (err) {
      throw new CliError({
        code: 'not_found',
        message: `cannot read file at ${input.path}: ${
          err instanceof Error ? err.message : String(err)
        }`,
        retriable: false,
      });
    }
    const url =
      this.env.serverUrl +
      internalPaths.agentUpdateAttachment(this.env.agentId, input.attachmentId);
    const form = new FormData();
    // filename + content_type are LOCKED on update (see the backend service):
    // the server ignores this Blob's name/type and reuses the row's, and
    // re-verifies the new bytes are the same magic family. A type hint here
    // would be a dead input, so none is sent.
    const blob = new Blob([buffer]);
    form.append('file', blob, basename(input.path));
    let res: Response;
    markRequestAttempted();
    try {
      res = await fetch(url, {
        method: 'PATCH',
        headers: {
          [AUTH_TOKEN_HEADER]: `Bearer ${this.env.authToken}`,
          'X-Agent-Hub-Caller': 'silverhand-cli',
        },
        body: form,
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    return (await res.json()) as AttachmentDto;
  }

  locateAttachment(attachmentId: string): Promise<AttachmentLocateDto> {
    return this.request('GET', internalPaths.agentLocateAttachment(this.env.agentId, attachmentId));
  }

  async downloadAttachment(attachmentId: string): Promise<{
    buffer: Buffer;
    contentType: string;
  }> {
    markRequestAttempted();
    const url =
      this.env.serverUrl + internalPaths.agentDownloadAttachment(this.env.agentId, attachmentId);
    let res: Response;
    try {
      res = await fetch(url, {
        method: 'GET',
        headers: this.headers(),
      });
    } catch (err) {
      throw fromNetworkError(err);
    }
    if (!res.ok) {
      const text = await safeReadText(res);
      throw mapHttpError(res.status, text);
    }
    const buffer = Buffer.from(await res.arrayBuffer());
    return {
      buffer,
      contentType: res.headers.get('content-type') ?? 'application/octet-stream',
    };
  }
}

async function safeReadText(res: Response): Promise<string> {
  try {
    return await res.text();
  } catch {
    return '';
  }
}

/** Encode a query map to a URL string. Array values become repeated keys
 *  (`?key=a&key=b`), the canonical multi-value form Hono's
 *  `c.req.queries(name)` reads. Skip undefined and empty arrays. */
function buildQueryString(
  query: Record<string, string | number | readonly string[] | readonly number[] | undefined>,
): string {
  const parts: string[] = [];
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined) continue;
    if (Array.isArray(value)) {
      if (value.length === 0) continue;
      for (const v of value) {
        parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(v))}`);
      }
    } else {
      parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
    }
  }
  return parts.length === 0 ? '' : `?${parts.join('&')}`;
}
