import { useEffect, useState, type JSX } from "react";
import { CheckIcon, CopyIcon, TerminalIcon, ViewIcon, ViewOffIcon } from "@/icons";
import { assembleConnectCommand } from "@silverhand/shared";
import { computerCopy } from "../../../features/computers/model/computer-copy";

interface ConnectionCommandCardProps {
  /** Server-owned launch recipe (npm spec) from the bootstrap payload —
   *  passed straight to `assembleConnectCommand`. */
  daemonNpmSpec: string;
  /** The reveal-once connect credential minted by the server. Assembled INTO
   *  the command here, and also used to locate + mask the secret span for the
   *  masked view. Never persisted, never cached. */
  connectToken: string;
  /** This desktop's OWN connected server URL (`BackendInfo.url`) — the
   *  proven-reachable origin it already talks to, and the SOLE source of the
   *  daemon's reach URL. Never the server's bind address. */
  connectedServerUrl: string;
  /** Header-bar label. Defaults to "Connection command". */
  label?: string;
}

export function ConnectionCommandCard({
  daemonNpmSpec,
  connectToken,
  connectedServerUrl,
  label = computerCopy().connectionCommandLabel,
}: ConnectionCommandCardProps): JSX.Element {
  const [revealed, setRevealed] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const t = window.setTimeout(() => setCopied(false), 1800);
    return () => window.clearTimeout(t);
  }, [copied]);

  // The reach URL is ALWAYS this desktop's own connected server URL — the
  // single source, so the two callers can never diverge and the server bind
  // address never reaches here. Assembled locally (see docblock). Empty only
  // for the sub-second tick before BackendInfo resolves — guarded so we never
  // render a malformed `--server-url` with no value or copy a broken command.
  const daemonCommand = connectedServerUrl
    ? assembleConnectCommand({
        daemonNpmSpec,
        serverUrl: connectedServerUrl,
        connectToken,
      })
    : "";

  const displayCommand = revealed ? daemonCommand : maskToken(daemonCommand, connectToken);

  const handleCopy = async (): Promise<void> => {
    if (!daemonCommand) return;
    // Copy source is ALWAYS the locally-assembled verbatim command.
    await navigator.clipboard.writeText(daemonCommand);
    setCopied(true);
  };

  return (
    <div className="flex flex-col gap-3">
      <div
        data-terminal-surface
        className="overflow-hidden rounded-(--floating-radius) border border-(--border-terminal) bg-(--bg-terminal)"
      >
        <div className="flex items-center justify-between gap-2 border-b border-(--border-terminal) bg-(--bg-terminal-chrome) px-3 py-1.5">
          <span className="inline-flex items-center gap-1.5 font-mono text-(length:--text-meta) uppercase tracking-wider text-(--text-terminal-chrome)">
            <TerminalIcon className="size-3.5" aria-hidden />
            {label}
          </span>
          <div className="flex items-center gap-1">
            <button
              type="button"
              onClick={() => setRevealed((v) => !v)}
              disabled={!daemonCommand}
              className="inline-flex items-center gap-1 px-1.5 py-1 font-mono text-(length:--text-meta) text-(--text-terminal-chrome) transition-colors hover:text-white disabled:cursor-not-allowed disabled:opacity-30"
              aria-label={revealed ? computerCopy().hide : computerCopy().reveal}
            >
              {revealed ? <ViewOffIcon className="size-3.5" /> : <ViewIcon className="size-3.5" />}
              {revealed ? computerCopy().hide : computerCopy().reveal}
            </button>
            <button
              type="button"
              onClick={() => void handleCopy()}
              disabled={!daemonCommand}
              className="inline-flex items-center gap-1 px-1.5 py-1 font-mono text-(length:--text-meta) text-(--text-terminal-chrome) transition-colors hover:text-white disabled:cursor-not-allowed disabled:opacity-30"
              aria-label={computerCopy().copy}
            >
              {copied ? <CheckIcon className="size-3.5" /> : <CopyIcon className="size-3.5" />}
              {copied ? computerCopy().copied : computerCopy().copy}
            </button>
          </div>
        </div>
        <div className="max-h-32 overflow-y-auto px-3 py-2.5">
          <code className="block whitespace-pre-wrap break-all font-mono text-xs leading-relaxed text-(--text-terminal)">
            <span className="select-none opacity-50" aria-hidden>
              ${" "}
            </span>
            {displayCommand}
          </code>
        </div>
      </div>

      <p className="text-xs leading-relaxed text-text-muted">{computerCopy().revealOnceNotice}</p>
    </div>
  );
}

export function maskToken(command: string, token: string): string {
  if (!token) return command;
  return command.split(token).join("*".repeat(12));
}
