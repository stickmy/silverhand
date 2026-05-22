/**
 * Thin HTTP client to the Silverhand backend's `/internal/conversations/
 * :cid/agents/:aid/...` route family.
 *
 * All failures funnel through `errors.mapHttpError` so the agent sees
 * stable codes instead of raw HTTP. Bearer auth uses the
 * `SILVERHAND_AUTH_TOKEN` value loaded from env; the per-(cid, aid)
 * binding is enforced server-side.
 */
import {
  AUTH_TOKEN_HEADER,
  internalPaths,
  type AgentListChannelsDto,
  type AgentListWorkspacesDto,
  type AgentReadHistoryDto,
  type AgentReadHistoryInput,
  type AgentSearchMessagesDto,
  type AgentSearchMessagesInput,
  type AttachmentDto,
  type AttachmentLocateDto,
  type ClaimTasksInput,
  type ClaimTasksResult,
  type CreateTasksInput,
  type CreateTasksResult,
  type ScheduleCancelInput,
  type ScheduleCancelOutput,
  type ScheduleCreateInput,
  type ScheduleCreateOutput,
  type ScheduleListInput,
  type ScheduleListOutput,
  type SendMessageTarget,
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
    method: 'GET' | 'POST',
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
    target?: SendMessageTarget;
    attachment_ids?: string[];
  }): Promise<{ message_id: string }> {
    return this.request(
      'POST',
      internalPaths.agentSendConversationMessage(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  createThread(input: {
    title: string;
    members?: string[];
    context_summary: string;
    initial_prompt: string;
  }): Promise<{ thread_id: string }> {
    return this.request(
      'POST',
      internalPaths.agentCreateThread(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  closeThread(input: {
    summary: string;
    posted_to_parent?: boolean;
    allow_open_tasks?: boolean;
  }): Promise<{ thread: unknown }> {
    return this.request(
      'POST',
      internalPaths.agentCloseThread(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  createChannel(input: {
    title: string;
    context_summary: string;
    initial_prompt: string;
  }): Promise<{ channel_id: string }> {
    return this.request(
      'POST',
      internalPaths.agentCreateChannel(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  readHistory(input: AgentReadHistoryInput): Promise<AgentReadHistoryDto> {
    return this.request(
      'POST',
      internalPaths.agentReadHistory(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  listWorkspaces(): Promise<AgentListWorkspacesDto> {
    return this.request(
      'GET',
      internalPaths.agentListWorkspaces(this.env.conversationId, this.env.agentId),
    );
  }

  listChannels(input: { workspace_id?: string } = {}): Promise<AgentListChannelsDto> {
    return this.request(
      'GET',
      internalPaths.agentListChannels(this.env.conversationId, this.env.agentId),
      undefined,
      { workspace_id: input.workspace_id },
    );
  }

  searchMessages(
    input: AgentSearchMessagesInput,
  ): Promise<AgentSearchMessagesDto> {
    return this.request(
      'POST',
      internalPaths.agentSearchMessages(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  // ── Tasks ────────────────────────────────────────────────────────
  listTasks(filters: {
    workspace_id?: string;
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
      internalPaths.agentListConversationTasks(this.env.conversationId, this.env.agentId),
      undefined,
      {
        workspace_id: filters.workspace_id,
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
      internalPaths.agentCreateConversationTasks(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  claimTasks(input: ClaimTasksInput): Promise<ClaimTasksResult> {
    return this.request(
      'POST',
      internalPaths.agentClaimConversationTasks(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  unclaimTask(task_number: number): Promise<TaskMutationResult> {
    return this.request(
      'POST',
      internalPaths.agentUnclaimConversationTask(this.env.conversationId, this.env.agentId),
      { task_number },
    );
  }

  updateTaskStatus(input: {
    task_number: number;
    status: TaskStatus;
  }): Promise<TaskMutationResult> {
    return this.request(
      'POST',
      internalPaths.agentUpdateConversationTaskStatus(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  // ── Schedules ────────────────────────────────────────────────────
  createSchedule(input: ScheduleCreateInput): Promise<ScheduleCreateOutput> {
    return this.request(
      'POST',
      internalPaths.agentScheduleCreate(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  listSchedules(input: ScheduleListInput): Promise<ScheduleListOutput> {
    return this.request(
      'POST',
      internalPaths.agentScheduleList(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  cancelSchedule(input: ScheduleCancelInput): Promise<ScheduleCancelOutput> {
    return this.request(
      'POST',
      internalPaths.agentScheduleCancel(this.env.conversationId, this.env.agentId),
      input,
    );
  }

  // ── Attachments ──────────────────────────────────────────────────
  /** Upload a file from disk. Backend derives channel ownership from
   *  the calling conversation; the agent never specifies channel id. */
  async uploadAttachment(input: {
    path: string;
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
    const url =
      this.env.serverUrl +
      internalPaths.agentUploadAttachment(this.env.conversationId, this.env.agentId);
    const form = new FormData();
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

  locateAttachment(attachmentId: string): Promise<AttachmentLocateDto> {
    return this.request(
      'GET',
      internalPaths.agentLocateAttachment(this.env.conversationId, this.env.agentId, attachmentId),
    );
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
