import { app, BrowserWindow, dialog, Notification, shell } from "electron";
import type { GuardedIpcRegistrar } from "./guarded-ipc.js";
import { mkdirSync, writeFileSync } from "node:fs";
import { materializeAndOpenAttachment } from "./attachment-open.js";
import { handleAttachmentSave } from "./attachment-save.js";
import { setMenuBarUnread } from "../shell/menu-bar.js";

export function registerSurfaceIpcHandlers(ipc: GuardedIpcRegistrar): void {
  // Live OS notifications keyed by the renderer-supplied notification id (the
  // backend notification row id). Retained so `notification:dismiss` can
  // `.close()` the exact alert when the backend reports it was read.
  // Entries self-evict on the Notification's own `close` event.
  const shownNotifications = new Map<string, Notification>();

  ipc.handle("dialog:pickDirectory", async (event, options?: { defaultPath?: string }) => {
    const win = BrowserWindow.fromWebContents(event.sender);
    const result = await dialog.showOpenDialog(win ?? undefined!, {
      properties: ["openDirectory", "createDirectory"],
      defaultPath: options?.defaultPath,
    });
    if (result.canceled || result.filePaths.length === 0) return null;
    return result.filePaths[0] ?? null;
  });

  ipc.handle("app:getVersion", () => app.getVersion());

  ipc.handle("shell:openExternal", async (_event, url: string) => {
    if (typeof url !== "string") return;
    // Refuse anything that isn't an http(s) link — preload is a security
    // boundary; agent-rendered content can hand us arbitrary strings.
    if (!/^https?:\/\//i.test(url)) return;
    await shell.openExternal(url);
  });

  /**
   * Hand an attachment off to the OS default app for that MIME (e.g. open
   * `.html` in the user's browser, `.pdf` in Preview). The renderer fetches
   * current bytes via the authenticated backend route on every open and ships
   * them here; we materialize to a single stable per-id scratch path and call
   * `shell.openPath`. Reopening an agent-updated attachment overwrites that
   * same file rather than spawning a new temp, so external open always shows
   * the latest bytes without accumulating stale copies. macOS resolves the
   * default handler from the on-disk extension, so the path + gating contract
   * (id validation, filename sanitize, `isSafeOpenExtension`) lives in the
   * pure `attachment-open` seam.
   *
   * Not tracking `content_revision` to live-rewrite an already-open external
   * file is deliberate: external open refreshes on reopen only; live
   * auto-update is the in-app tab's job.
   *
   * Why not `window.open(blob_url)` / a fresh BrowserWindow? `shell.openExternal`
   * can't resolve a renderer-scoped `blob:` URL (the original `.html` bug,
   * and keeping untrusted HTML out of any Electron WebContents is
   * simpler than sandboxing it — the OS browser already provides origin
   * isolation and CSP.
   */
  ipc.handle(
    "attachment:openExternal",
    async (
      _event,
      input: { attachmentId?: unknown; filename?: unknown; bytes?: unknown },
    ): Promise<{ ok: true } | { ok: false; reason: string }> => {
      const bytesRaw = input?.bytes;
      if (!(bytesRaw instanceof Uint8Array) && !Buffer.isBuffer(bytesRaw)) {
        return { ok: false, reason: "invalid_payload" };
      }
      const bytes = Buffer.isBuffer(bytesRaw) ? bytesRaw : Buffer.from(bytesRaw);
      return materializeAndOpenAttachment(
        { attachmentId: input?.attachmentId, filename: input?.filename, bytes },
        {
          ensureDir: (dir) => mkdirSync(dir, { recursive: true }),
          writeFile: (path, data) => writeFileSync(path, data, { mode: 0o600 }),
          openPath: (path) => shell.openPath(path),
        },
      );
    },
  );

  /**
   * Save an attachment to a user-chosen location via the OS save-as
   * dialog. Same trust boundary as `attachment:openExternal` — main
   * owns the filesystem write; the renderer only forwards the bytes it
   * already fetched through the authenticated backend route.
   *
   * Result shape distinguishes cancel from failure so the renderer can
   * stay silent on cancel (a normal user action, not an error) while
   * still surfacing real write failures.
   *
   * No `isSafeOpenExtension` gate here — the user is choosing where
   * bytes land on their own disk, not asking the OS to *execute* the
   * file. The renderer-supplied filename is sanitized (path separators
   * and control chars stripped) and pre-fills the dialog; the user has
   * final say over the destination path and extension.
   */
  ipc.handle("attachment:save", async (event, input: { filename?: unknown; bytes?: unknown }) => {
    const win = BrowserWindow.fromWebContents(event.sender) ?? undefined;
    return handleAttachmentSave(input, {
      showSaveDialog: (options) => dialog.showSaveDialog(win!, options),
      writeFile: async (path, data) => {
        const { writeFile } = await import("node:fs/promises");
        await writeFile(path, data);
      },
    });
  });

  ipc.handle("badge:setCount", (_event, value: unknown) => {
    // macOS dock badge. n=0 clears it; non-finite / negative values are
    // coerced to 0 so a buggy renderer can't paint nonsense on the icon.
    // Renderer drives this from `useMessageUnreadSummary` (sum across
    // workspaces) — no other source feeds the badge.
    const count =
      typeof value === "number" && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
    app.setBadgeCount(count);
    setMenuBarUnread(count);
  });

  ipc.handle("notification:show", (event, opts: unknown) => {
    if (!opts || typeof opts !== "object") return;
    const { title, subtitle, body, groupId, force, notificationId } = opts as Partial<
      Record<string, unknown>
    >;
    if (typeof title !== "string") return;
    // Fire ordinary system notifications only when no Electron window is
    // focused. Direct human mentions opt into `force` so an active-but-
    // different channel still produces an OS notification instead of only
    // changing the unread dot.
    if (force !== true && BrowserWindow.getFocusedWindow() !== null) return;
    const n = new Notification({
      title,
      subtitle: optionalString(subtitle),
      body: optionalString(body),
      groupId: optionalString(groupId),
    });
    // Click handler — only wired when the renderer attached a
    // `notificationId`. Bring a window forward (the user clicked, that's
    // the intent) and forward the id back so the renderer can navigate
    // to the right conversation. Without an id this is just an alert and
    // we keep the legacy "show only" behavior.
    if (typeof notificationId === "string" && notificationId.length > 0) {
      // Retain so a later `notification:dismiss` (backend reported it read) can
      // close this exact alert; self-evict when the OS closes it.
      shownNotifications.set(notificationId, n);
      n.on("close", () => {
        if (shownNotifications.get(notificationId) === n) {
          shownNotifications.delete(notificationId);
        }
      });
      n.on("click", () => {
        // Prefer the sender's window (the renderer that called us); fall
        // back to any visible window. macOS keeps windows around in the
        // dock even when minimized, so showing the first one is safe.
        const senderWindow = BrowserWindow.fromWebContents(event.sender);
        const target =
          senderWindow ?? BrowserWindow.getAllWindows().find((w) => !w.isDestroyed()) ?? null;
        if (target) {
          if (target.isMinimized()) target.restore();
          target.show();
          target.focus();
        }
        // Best-effort focus the app itself (e.g. when another app is
        // frontmost). On macOS this nudges Silverhand to the foreground.
        if (process.platform === "darwin") app.focus({ steal: true });
        if (!event.sender.isDestroyed()) {
          event.sender.send("notification:clicked", notificationId);
        }
      });
    }
    n.show();
  });

  ipc.handle("notification:dismiss", (_event, notificationIds: unknown) => {
    // Close OS notifications the backend reported as read (#task:1431). The
    // renderer forwards the exact ids from the viewer-scoped "notification read"
    // fact — main never re-derives read state. Unknown / already-closed ids are
    // silently ignored (best-effort: OS alerts are transient).
    if (!Array.isArray(notificationIds)) return;
    for (const id of notificationIds) {
      if (typeof id !== "string") continue;
      const n = shownNotifications.get(id);
      if (n) {
        n.close();
        shownNotifications.delete(id);
      }
    }
  });
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
