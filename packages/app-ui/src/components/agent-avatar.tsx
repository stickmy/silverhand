import type { RuntimeKind } from "@silverhand/shared";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  AntigravityIcon,
  ClaudeIcon,
  CodexIcon,
  CursorIcon,
  GeminiIcon,
  GrokIcon,
  OPENCODE_LOGO_DATA_URI,
  PiIcon,
  QODER_LOGO_DATA_URI,
} from "@/icons/agents";
import { cn } from "@/lib/cn";
import { AVATAR_PX_DEFAULT } from "./avatar-scale";

interface AgentAvatarProps {
  runtime: RuntimeKind;
  /** Pixel size of the square. Sizes come from the `AVATAR_PX` scale
   *  (#task:1440); defaults to `md` (28). */
  size?: number;
  className?: string;
  title?: string;
  /**
   * When provided and non-null, render the bytes as the avatar image
   * (rounded to the same silhouette as the runtime fallback). When
   * null/undefined, render the runtime icon. The URL is expected to
   * be an `URL.createObjectURL(...)` result owned by the caller —
   * this component never fetches or revokes it.
   *
   * Wiring rationale: avoiding internal fetches here is a firm PR-2
   * constraint. The same agent rendered in 100 message rows would
   * otherwise trigger 100 fetches and 100 object URL allocations;
   * the parent uses `useAgentAvatarObjectUrl` (single) or
   * `useAgentAvatarObjectUrls` (batch) and threads the resolved URL
   * down as a prop.
   */
  avatarObjectUrl?: string | null;
  /** Alt text rendered when an avatar image is shown. Falls back to
   *  the agent display_name or runtime name. */
  alt?: string;
}

// Monochrome vector marks, tinted via `currentColor` on a soft badge.
// Runtimes with a full-bleed raster brand logo (see `BRAND_LOGOS`) are
// NOT here — they render edge-to-edge and skip the tinted badge.
const ICONS: Partial<Record<RuntimeKind, (p: { style?: React.CSSProperties }) => JSX.Element>> = {
  claude: ClaudeIcon,
  codex: CodexIcon,
  cursor: CursorIcon,
  gemini: GeminiIcon,
  pi: PiIcon,
  grok: GrokIcon,
  antigravity: AntigravityIcon,
};

// Full-color brand logos that carry their own background + corner
// radius — rendered edge-to-edge (like an uploaded avatar), no badge tint.
const BRAND_LOGOS: Partial<Record<RuntimeKind, string>> = {
  opencode: OPENCODE_LOGO_DATA_URI,
  qoder: QODER_LOGO_DATA_URI,
};

const TINTS: Partial<Record<RuntimeKind, { bg: string; fg: string }>> = {
  grok: { bg: "bg-[#09090b]", fg: "text-white" },
  antigravity: { bg: "bg-[#ffffff]", fg: "text-[#3186ff]" },
  pi: { bg: "bg-[#09090b]", fg: "text-white" },
  claude: { bg: "bg-[#fdf3ec]", fg: "text-[#d97757]" },
  codex: { bg: "bg-[#1f1f1f]", fg: "text-white" },
  cursor: { bg: "bg-[#0f0f0f]", fg: "text-white" },
  gemini: { bg: "bg-[#e8f0fe]", fg: "text-[#1a73e8]" },
};

export function AgentAvatar({
  runtime,
  size = AVATAR_PX_DEFAULT,
  className,
  title,
  avatarObjectUrl,
  alt,
}: AgentAvatarProps): JSX.Element {
  // When an uploaded avatar URL is available, render the image and
  // skip the runtime tint — the bytes already encode the agent's
  // identity so the brand-color frame would be visual noise.
  if (avatarObjectUrl) {
    return (
      <span
        data-avatar-ground
        className={cn(
          "inline-flex shrink-0 items-center justify-center overflow-hidden rounded-(--avatar-radius) align-middle",
          className,
        )}
        style={{ width: size, height: size }}
        title={title}
      >
        <img
          src={avatarObjectUrl}
          alt={alt ?? title ?? `${runtime} avatar`}
          className="h-full w-full object-cover"
          draggable={false}
        />
      </span>
    );
  }

  // Full-bleed brand logo (own background + corners) — render edge-to-edge
  // like an uploaded avatar, no tinted badge.
  const brandLogo = BRAND_LOGOS[runtime];
  if (brandLogo) {
    return (
      <span
        className={cn(
          "inline-flex shrink-0 items-center justify-center overflow-hidden rounded-(--avatar-radius) align-middle",
          className,
        )}
        style={{ width: size, height: size }}
        title={title}
        aria-hidden
      >
        <img src={brandLogo} alt="" className="h-full w-full object-cover" draggable={false} />
      </span>
    );
  }

  const Icon = ICONS[runtime];
  const tint = TINTS[runtime];
  const inner = runtime === "pi" ? size : Math.round(size * 0.6);

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center overflow-hidden rounded-(--avatar-radius) align-middle",
        tint?.bg,
        tint?.fg,
        className,
      )}
      style={{ width: size, height: size }}
      title={title}
      aria-hidden
    >
      {Icon ? <Icon style={{ width: inner, height: inner }} /> : null}
    </span>
  );
}

/** The face `AgentAvatar` shows, as an image URL for surfaces drawn outside
 * the page (the native bar); null for a runtime without a mark. */
export function agentAvatarImage(
  runtime: RuntimeKind,
  avatarObjectUrl: string | null,
): string | null {
  const image = avatarObjectUrl ?? BRAND_LOGOS[runtime];
  if (image) {
    return image;
  }

  const Icon = ICONS[runtime];
  const tint = TINTS[runtime];
  if (!Icon || !tint) {
    return null;
  }

  const hex = (token: string) => /#[0-9a-f]+/i.exec(token)?.[0] ?? "#000";
  const inner = runtime === "pi" ? 1 : 0.6;
  const mark = renderToStaticMarkup(createElement(Icon, {}));
  // An explicit size: WebKit reports a viewBox-only SVG at a default size it
  // does not draw at, so a canvas crop of it keeps only a corner.
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100" style="color:${hex(tint.fg)}"><rect width="100" height="100" fill="${hex(tint.bg)}"/><svg x="${50 - inner * 50}" y="${50 - inner * 50}" width="${inner * 100}" height="${inner * 100}">${mark}</svg></svg>`;
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}
