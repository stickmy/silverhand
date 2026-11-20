import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const READ_ENDPOINT = "/api/notifications/read";

function sliceFunctionBody(source: string, declaration: string): string {
  const start = source.indexOf(declaration);
  if (start === -1) {
    throw new Error(`could not find "${declaration}" — was the hook renamed?`);
  }
  // The hook ends where the next top-level function declaration begins.
  const rest = source.slice(start + declaration.length);
  const nextDecl = rest.search(/\n(?:export )?function /);
  return nextDecl === -1 ? rest : rest.slice(0, nextDecl);
}

describe("notification toast surface — surfaced ≠ eliminated", () => {
  const source = readFileSync(
    fileURLToPath(new URL("./notifications.ts", import.meta.url)),
    "utf8",
  );

  it("useNotificationToasts never writes notifications.read_at (no auto-read on toast)", () => {
    const body = sliceFunctionBody(source, "export function useNotificationToasts(");
    expect(body).not.toContain(READ_ENDPOINT);
  });

  it("the explicit useMarkNotificationsRead mutation still owns the read endpoint", () => {
    const body = sliceFunctionBody(source, "export function useMarkNotificationsRead(");
    expect(body).toContain(READ_ENDPOINT);
  });
});

describe("conversation focus surface — must not drain notification read_at", () => {
  const appSource = readFileSync(fileURLToPath(new URL("../../app.tsx", import.meta.url)), "utf8");
  const focusSelectorSource = readFileSync(
    fileURLToPath(new URL("../conversation-shell/use-conversation-panes.ts", import.meta.url)),
    "utf8",
  );
  const DRAIN_CALL = "markConversationNotificationsRead.mutate";

  it("app.tsx never invokes the notification drain directly (no focus-drain)", () => {
    // The Activity rail's channel/thread panes are the SAME route-driven
    // ChannelView/ThreadPanel as chat; notification clearing is an explicit
    // Activity-row action, not a shell effect. A re-introduced focus-drain
    // lands here.
    expect(appSource).not.toContain(`${DRAIN_CALL}(`);
  });

  it("the focus selector never wires the notification drain", () => {
    expect(focusSelectorSource).not.toContain(DRAIN_CALL);
  });
});
