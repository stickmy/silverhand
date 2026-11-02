import { InlineLoading } from "@/components/layout/inline-loading";
import { Fragment, useEffect, useRef } from "react";
import { HashIcon } from "@/icons";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import type { PickerItem } from "../model/composer-picker";
import { avatarUrlOf, type PickerRowView, type PickerView } from "../model/composer-picker-rows";

// ── menu ──────────────────────────────────────────────────────────

export function PickerMenu({
  view,
  active,
  onPick,
  onHover,
  agentAvatarUrls,
  humanAvatarUrls,
}: {
  view: PickerView;
  active: number;
  onPick: (item: PickerItem) => void;
  onHover: (index: number) => void;
  agentAvatarUrls: Map<string, string | null>;
  humanAvatarUrls: Map<string, string | null>;
}): JSX.Element {
  return (
    <div
      className="absolute bottom-full left-0 z-(--z-raised) mb-1.5 w-64 overflow-hidden rounded-(--select-content-radius) bg-bg-floating shadow-popover"
      data-composer-picker=""
      data-dropdown-frame
      onMouseDown={(e) => e.preventDefault()}
    >
      <div className="scrollbar-hide max-h-64 overflow-y-auto p-1" role="listbox">
        {view.rows.map((row, i) => (
          <Fragment key={row.key}>
            {row.section ? <SectionHeader label={row.section} /> : null}
            <Row
              row={row}
              active={i === active}
              onPick={() => onPick(row.item)}
              onHover={() => onHover(i)}
              avatarUrl={avatarUrlOf(row.item, agentAvatarUrls, humanAvatarUrls)}
            />
          </Fragment>
        ))}
      </div>
      {/* Outside the scroll area, so a short query learns more exist at any
          scroll position; informational only, not an option. */}
      {view.more ? (
        <div
          aria-hidden
          className="border-t border-border-subtle px-3 py-1.5 text-(length:--text-meta) text-text-muted"
        >
          {view.more}
        </div>
      ) : null}
      {view.status?.loading ? (
        <div className="border-t border-border-subtle">
          <InlineLoading label={view.status.label} />
        </div>
      ) : view.status ? (
        <div
          role="status"
          className="border-t border-border-subtle px-3 py-1.5 text-(length:--text-meta) text-text-muted"
        >
          {view.status.label}
        </div>
      ) : null}
    </div>
  );
}

export function SectionHeader({ label }: { label: string }): JSX.Element {
  return (
    <div
      role="presentation"
      className="px-2 pb-1 pt-1.5 text-(length:--text-badge) font-medium uppercase tracking-wider text-text-muted"
    >
      {label}
    </div>
  );
}

export function Row({
  row,
  active,
  onPick,
  onHover,
  avatarUrl,
}: {
  row: PickerRowView;
  active: boolean;
  onPick: () => void;
  onHover: () => void;
  avatarUrl: string | null;
}): JSX.Element {
  const { item } = row;
  const cls =
    "flex w-full items-center gap-2.5 rounded-sm px-2 py-1.5 text-left " +
    (active ? "bg-bg-hover text-text-primary" : "text-text-secondary") +
    (row.dimmed && !active ? " opacity-60" : "");

  const rowRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!active) return;
    rowRef.current?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <button
      ref={rowRef}
      type="button"
      role="option"
      aria-selected={active}
      onClick={onPick}
      onMouseMove={onHover}
      className={cls}
    >
      {item.kind === "agent" ? (
        <AgentAvatar
          runtime={item.agent.runtime}
          size={AVATAR_PX.md}
          avatarObjectUrl={avatarUrl}
          alt={row.title}
        />
      ) : item.kind === "human" ? (
        <UserAvatar size={AVATAR_PX.md} avatarObjectUrl={avatarUrl} alt={row.title} />
      ) : (
        <HashIcon className="size-4 shrink-0 text-text-muted" />
      )}
      {row.handle ? (
        <div className="flex min-w-0 flex-1 flex-col leading-tight">
          <span className="truncate text-sm font-medium text-text-primary">{row.title}</span>
          <div className="flex items-center gap-1.5 text-(length:--text-meta) text-text-muted">
            <span className="truncate font-mono">{row.handle}</span>
            <span
              data-agent-runtime={item.kind === "agent" ? "" : undefined}
              data-participant-kind={item.kind === "human" ? "" : undefined}
              className="shrink-0"
            >
              {row.detail}
            </span>
          </div>
        </div>
      ) : row.detail ? (
        <div className="flex min-w-0 flex-1 flex-col leading-tight">
          <span className="truncate text-sm font-medium text-text-primary">{row.title}</span>
          <span className="truncate text-(length:--text-meta) text-text-muted">{row.detail}</span>
        </div>
      ) : (
        <span className="truncate text-sm font-medium text-text-primary">{row.title}</span>
      )}
    </button>
  );
}
