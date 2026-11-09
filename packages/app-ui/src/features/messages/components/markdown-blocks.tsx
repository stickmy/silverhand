import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronDownIcon, ChevronUpIcon } from "@/icons";
import { t } from "@/lib/i18n";
import { CopyFeedbackIcon } from "./copy-feedback-icon";

const OVERSIZED_PREVIEW_CHARS = 4000;

/** Bounded plain-text rendering for bodies over the render budget (legacy
 *  oversized rows). Never touches the markdown pipeline; full content stays
 *  retrievable via copy. */
export function OversizedContent({
  content,
  bytes,
}: {
  content: string;
  bytes: number;
}): JSX.Element {
  const [copied, setCopied] = useState(false);
  const onCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(content);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard denied; ignore */
    }
  }, [content]);
  const label =
    bytes >= 1024 * 1024
      ? `${(bytes / (1024 * 1024)).toFixed(1)} MB`
      : `${Math.round(bytes / 1024)} KB`;
  return (
    <div className="markdown-body">
      <div className="flex items-center gap-2 text-xs opacity-60">
        <span>
          {label} {t("messages.tooLargeToRender")}
        </span>
        <button type="button" onClick={onCopy} className="underline hover:opacity-80">
          {copied ? t("messages.copied") : t("messages.copyFullText")}
        </button>
      </div>
      <pre className="whitespace-pre-wrap wrap-break-word">
        {content.slice(0, OVERSIZED_PREVIEW_CHARS)}
        {"\n…"}
      </pre>
    </div>
  );
}

export function CodeBlock({ children }: { children: React.ReactNode }): JSX.Element {
  const [copied, setCopied] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [overflowing, setOverflowing] = useState(false);
  const preRef = useRef<HTMLPreElement>(null);

  const lang = extractLang(children);

  const onCopy = useCallback(async () => {
    const text = extractText(children);
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard denied; ignore */
    }
  }, [children]);

  // Detect whether the content actually exceeds the cap so we only
  // surface the Expand affordance when it would do something. Re-runs
  // on every render — the body content is the children prop, which
  // changes when streaming append-only edits land.
  useEffect(() => {
    const el = preRef.current;
    if (!el) return;
    setOverflowing(el.scrollHeight > el.clientHeight + 1);
  }, [children, expanded]);

  return (
    <div className={"code-block" + (expanded ? " code-block--expanded" : "")}>
      <div className="code-block__header">
        <span className="code-block__lang">{lang ?? "text"}</span>
        <button
          type="button"
          onClick={onCopy}
          className="code-block__copy"
          title={copied ? t("messages.copied") : t("messages.copyCode")}
          aria-label={t("messages.copyCode")}
        >
          {/* Icon only — the header strip is chrome, and a word next to a
              glyph that already says "copy" spends label width on nothing
              (shiki #task:1835). The Check/Copy swap carries the copied
              confirmation on its own; the accessible name stays on
              `aria-label` (constant, so screen readers announce the action)
              while `title` carries the transient "Copied" state. */}
          <CopyFeedbackIcon copied={copied} />
        </button>
      </div>
      <pre ref={preRef}>{children}</pre>
      {overflowing || expanded ? (
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="code-block__expand"
          aria-label={expanded ? t("messages.collapseCode") : t("messages.expandCode")}
          aria-expanded={expanded}
          title={expanded ? t("messages.collapseCode") : t("messages.expandCode")}
        >
          {expanded ? (
            <ChevronUpIcon size={16} aria-hidden="true" />
          ) : (
            <ChevronDownIcon size={16} aria-hidden="true" />
          )}
        </button>
      ) : null}
    </div>
  );
}

/** Pull `language-xxx` off the (singular) child `<code>` element that
 *  react-markdown places inside every fenced `pre`. Returns null for
 *  unfenced blocks or when the agent omitted the info string. */
export function extractLang(children: React.ReactNode): string | null {
  if (
    children &&
    typeof children === "object" &&
    "props" in children &&
    typeof (children as { props?: { className?: unknown } }).props?.className === "string"
  ) {
    const cls = (children as { props: { className: string } }).props.className;
    const m = /language-([\w+-]+)/.exec(cls);
    if (m) return m[1] ?? null;
  }
  return null;
}

export function extractText(node: React.ReactNode): string {
  if (typeof node === "string") return node;
  if (typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (
    node &&
    typeof node === "object" &&
    "props" in node &&
    (node as { props?: { children?: React.ReactNode } }).props?.children !== undefined
  ) {
    return extractText((node as { props: { children: React.ReactNode } }).props.children);
  }
  return "";
}
