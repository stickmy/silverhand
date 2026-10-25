/**
 * `silverhand file` — upload, update, view.
 */
import { constants } from 'node:fs';
import { access, mkdir, rename, stat, writeFile } from 'node:fs/promises';
import { basename, join } from 'node:path';
import { type AttachmentLocateDto } from '@silverhand/shared';
import { defaultAppRoot, resolveDesktopAppPaths } from '@silverhand/shared/app-data-layout';
import { CliError } from '../errors.js';
import { flagString, type ParsedArgs } from '../parse.js';
import { requireTargetFlag } from '../target-flag.js';
import { reachHelpErrorRow } from '../reach-help.js';
import type { Command } from './index.js';

/** The one positional operand a file verb takes, as an alternative spelling of
 *  its flag. Both together, or more than one positional, is refused. */
function operandOrFlag(args: ParsedArgs, verb: string, flag: string): string | undefined {
  const fromFlag = flagString(args, flag);
  const tokens = args.positional;
  if (tokens.length === 0) return fromFlag;
  if (tokens.length > 1 || fromFlag !== undefined) {
    throw new CliError({
      code: 'validation_error',
      subkind: 'unexpected_arguments',
      message:
        `silverhand file ${verb} takes ONE <${flag}>, either as --${flag} or as the ` +
        `positional operand; got ${JSON.stringify(tokens)}` +
        (fromFlag !== undefined ? ` and --${flag}` : '') +
        '.',
      retriable: false,
      details: { command: `file ${verb}`, flag, positional: tokens.slice() },
    });
  }
  return tokens[0];
}

const uploadCommand: Command = {
  verb: 'upload',
  summary: 'Upload a local file as an attachment of an addressed conversation.',
  help: `silverhand file upload --target <address> (--path <abs_path> | <abs_path>)
                        [--filename <name>] [--content-type <mime>]

Reads the file at --path (must be absolute) and uploads it as an
attachment of the conversation --target names. Returns an attachment id
you pass to \`silverhand message send --attachment-id <id>\` — send takes
its OWN --target, so upload where you are going to send.

An attachment belongs to a CONVERSATION and threads inherit their
parent's files, so --target must name a conversation: a
\`/thread:<root_message_id>\` suffix is refused
(\`thread_suffix_not_supported\`) rather than silently dropped. Upload to
the parent and send the message into the thread.

Flags:
  --target         Required. The conversation to attach the file to, e.g.
                   \`#conv:<conversation_id>\`,
                   \`dm:@<handle>\`, or
                   \`#<workspace>/<title>\`. Copy the \`target=\` token from a
                   message header when it is the stream you mean. One you cannot post to is refused
                   exactly like one that does not exist.
  --path           Required. Absolute path on local disk; may also be
                   given as the positional operand.
  --filename       Optional display filename. Defaults to basename(path).
                   The backend sanitizes path separators and control chars.
  --content-type   Optional MIME hint. Backend sniffs magic bytes and
                   falls back to file extension when omitted.

stdout: AttachmentDto JSON.

Errors:
  target_required (validation)       --target was omitted
  target_malformed (400)             --target is not an address; the
                                     resolver answers this, so the message
                                     names the accepted forms
  target_unresolved (404)            --target names no stream you can
                                     write to — the same answer an id
                                     nobody holds gets
  thread_suffix_not_supported (400)  --target named a reply cluster
${reachHelpErrorRow('file_upload', 37)}  capability_denied (403)            you lack \`upload_attachments\` where
                                     --target points`,
  flags: ['target', 'path', 'filename', 'content-type'] as const,
  allowPositional: true,
  async run({ env, client, args }) {
    const target = requireTargetFlag(flagString(args, 'target'), env, {
      command: 'silverhand file upload',
      because:
        'an attachment is owned by ONE conversation, and the conversation this ' +
        'CLI is authenticated in is where your credential lives, not a default ' +
        'destination.',
    });
    const path = operandOrFlag(args, 'upload', 'path');
    if (!path) {
      throw new CliError({
        code: 'validation_error',
        message: '--path is required',
        retriable: false,
      });
    }
    const filename = flagString(args, 'filename');
    const contentType = flagString(args, 'content-type');
    return client.uploadAttachment({
      path,
      target,
      ...(filename ? { filename } : {}),
      ...(contentType ? { contentType } : {}),
    });
  },
};

const updateCommand: Command = {
  verb: 'update',
  summary: 'Replace the bytes of an attachment you uploaded, keeping its id.',
  help: `silverhand file update (--attachment-id <id> | <id>) --path <abs_path>

Replace the CONTENT of an attachment you previously uploaded, in place.
The attachment id, filename, and content-type stay fixed — only the bytes
change. Use this when iterating on an artifact (e.g. a mockup) so a
reviewer's resolved local path from \`silverhand file view\` stays valid
across rounds instead of changing every re-upload.

Creator-only: you can update only attachments you uploaded, and only from
within their channel. There is no versioning — the previous bytes are
replaced, not retained.

The content type is LOCKED: the new file must be the same kind as the
original (the server verifies it from the bytes), so this can refresh a
PNG mockup but not turn it into a PDF. To change type, upload a new
attachment.

Flags:
  --attachment-id  Required. The attachment id to update; may also be
                   given as the positional operand.
  --path           Required. Absolute path to the new local file.

stdout: AttachmentDto JSON (same id, bumped content_revision).`,
  flags: ['attachment-id', 'path'] as const,
  allowPositional: true,
  async run({ client, args }) {
    const attachmentId = operandOrFlag(args, 'update', 'attachment-id');
    if (!attachmentId) {
      throw new CliError({
        code: 'validation_error',
        message: '--attachment-id is required',
        retriable: false,
      });
    }
    const path = flagString(args, 'path');
    if (!path) {
      throw new CliError({
        code: 'validation_error',
        message: '--path is required',
        retriable: false,
      });
    }
    return client.updateAttachment({ attachmentId, path });
  },
};

const viewCommand: Command = {
  verb: 'view',
  summary: 'Resolve an attachment id to a readable local path.',
  help: `silverhand file view (--attachment-id <id> | <id>)

Resolve an attachment id to a local file path readable by this agent.
Cross-channel ids are rejected.

stdout: AttachmentLocateDto JSON:
  { storage: "local", local_path, filename, content_type }`,
  flags: ['attachment-id'] as const,
  allowPositional: true,
  async run({ env, client, args }) {
    const attachmentId = operandOrFlag(args, 'view', 'attachment-id');
    if (!attachmentId) {
      throw new CliError({
        code: 'validation_error',
        message: '--attachment-id is required',
        retriable: false,
      });
    }
    const located = await client.locateAttachment(attachmentId);
    if (located.storage === 'local' && shouldTrustLocalPath(env.serverUrl)) {
      const readable = await canReadFile(located.local_path);
      if (readable) return located;
    }
    const downloaded = await client.downloadAttachment(attachmentId);
    const localPath = await writeAttachmentCache({
      agentId: env.agentId,
      attachmentId,
      filename: located.filename,
      buffer: downloaded.buffer,
    });
    return {
      storage: 'local',
      local_path: localPath,
      filename: located.filename,
      content_type: located.content_type || downloaded.contentType,
    } satisfies AttachmentLocateDto;
  },
};

export const fileCommands: readonly Command[] = [uploadCommand, updateCommand, viewCommand];

async function canReadFile(path: string): Promise<boolean> {
  try {
    const s = await stat(path);
    if (!s.isFile()) return false;
    await access(path, constants.R_OK);
    return true;
  } catch {
    return false;
  }
}

function shouldTrustLocalPath(serverUrl: string): boolean {
  try {
    const { hostname } = new URL(serverUrl);
    const normalized = hostname.toLowerCase();
    return (
      normalized === 'localhost' ||
      normalized === '::1' ||
      normalized === '[::1]' ||
      normalized === '0:0:0:0:0:0:0:1' ||
      normalized.startsWith('127.')
    );
  } catch {
    return false;
  }
}

async function writeAttachmentCache(input: {
  agentId: string;
  attachmentId: string;
  filename: string;
  buffer: Buffer;
}): Promise<string> {
  const dir = join(
    resolveDesktopAppPaths(defaultAppRoot()).attachmentsCache,
    safeSegment(input.agentId),
    safeSegment(input.attachmentId),
  );
  await mkdir(dir, { recursive: true });
  const filePath = join(dir, safeFilename(input.filename));
  const tmpPath = join(dir, `.${safeSegment(input.attachmentId)}.${process.pid}.${Date.now()}.tmp`);
  await writeFile(tmpPath, input.buffer, { mode: 0o600 });
  await rename(tmpPath, filePath);
  return filePath;
}

function safeSegment(value: string): string {
  const sanitized = value
    .trim()
    .replace(/[^a-zA-Z0-9._-]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return sanitized || 'unknown';
}

function safeFilename(value: string): string {
  const base = basename(value.trim() || 'attachment.bin')
    .replace(/[\x00-\x1F\x7F]/g, '_')
    .replace(/[\\/]/g, '_');
  return base || 'attachment.bin';
}
