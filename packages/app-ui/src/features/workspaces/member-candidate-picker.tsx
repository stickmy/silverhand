import { InlineLoading } from "@/components/layout/inline-loading";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { SearchIcon } from "@/icons";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { dropdownPanelProps } from "@/components/ui/dropdown-panel";
import { SEARCH_FIELD_FRAME } from "@/components/common/control-ink-border";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { cn } from "@/lib/cn";
import {
  candidateDisplayName,
  candidateHandle,
  candidateKey,
  matchesCandidate,
  normalizeMemberQuery,
  type MemberCandidate,
  type MemberCandidateGroup,
} from "./member-search";
import { t } from "@/lib/i18n";

/**
 * The single member picker — a search input that narrows a grouped candidate
 * list, one component for both places the product picks a member: the
 * workspace/titlebar roster (`ManageMembersPopover`) and the conversation
 * roster (`AddConversationMemberPopover`). It used to be two hand-rolled
 * copies, so adapting one (theme, keyboard nav, name budget) meant remembering
 * the other — the drift this merge removes.
 *
 * Why an input and not a `Select`: the candidate pool grows with the fleet, so
 * typing is the only interaction that scales. Search is the primary affordance
 * and the list is what it narrows. The caller supplies `groups` already
 * narrowed by its OWN domain rules (who is eligible, who is not yet a member);
 * this component narrows only by the typed QUERY, so the eligibility answer is
 * never re-derived here.
 *
 * `mode` is the only shape difference between the two call sites:
 *  - `floating`: the list is a plain absolutely-positioned dropdown that opens
 *    under the input on focus/type — used inside a compact popover whose height
 *    is just the input. It is NOT a portal/Radix layer: consumers mount this
 *    inside a Radix Popover, and a nested dismissable layer would let a click
 *    in the inner list dismiss the outer popover. Staying in the same layer
 *    also keeps the input focused while the list is open. In poster the panel
 *    wears the poster ink frame via `dropdownPanelProps`.
 *  - `inline`: the list is always visible below the input, a frameless scroll
 *    region inside a host popover that already supplies the frame — used where
 *    the popover is tall enough to show input and results together.
 *
 * Group order is the caller's (`groups` renders verbatim); a group with no
 * matches renders nothing, but a group that still has matches keeps its label.
 * Keyboard navigation walks the flat concatenation of all groups, so the
 * highlight never lands on a section label. Rows `preventDefault` on mousedown
 * so picking one never blurs the input first, and (floating) Escape is
 * swallowed while the list is open so the first Escape closes the list rather
 * than the host popover.
 */
export function MemberCandidatePicker({
  groups,
  onPick,
  mode,
  ariaLabel,
  emptyHint,
  loading = false,
  disabled = false,
  rowsDisabled = false,
  autoFocus = false,
  footer,
  onQueryChange,
  placeholder = t("workspaces.searchByNameOr"),
  inputId,
}: {
  /** Candidate pool, already narrowed by the caller's eligibility rules. */
  groups: readonly MemberCandidateGroup[];
  onPick: (candidate: MemberCandidate) => void;
  mode: "floating" | "inline";
  ariaLabel: string;
  /** Shown when the viewer has no eligible candidates at all (no query). */
  emptyHint: string;
  loading?: boolean;
  /** Disable the search input (floating: while an enrollment is in flight). */
  disabled?: boolean;
  /** Disable candidate rows (inline: while a pick is in flight, list stays up). */
  rowsDisabled?: boolean;
  autoFocus?: boolean;
  /** Rendered below the list, inside the picker's own column (inline errors). */
  footer?: ReactNode;
  onQueryChange?: (query: string) => void;
  placeholder?: string;
  /** Lets a form `<Label htmlFor>` target the search input. */
  inputId?: string;
}): JSX.Element {
  const inline = mode === "inline";
  const [query, setQuery] = useState("");
  // Inline mode is always open (input + list shown together); floating opens
  // on focus/type. A single `listOpen` drives rendering either way.
  const [open, setOpen] = useState(false);
  const listOpen = inline || open;
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  // `@alice` is how a handle is spelled everywhere else, and the placeholder
  // invites it — so the sigil is normalized away before matching. A bare `@`
  // is "no query yet", not "no matches".
  const normalizedQuery = normalizeMemberQuery(query);

  const matched = useMemo<MemberCandidateGroup[]>(() => {
    return groups
      .map((g) => ({
        label: g.label,
        items: normalizedQuery
          ? g.items.filter((c) => matchesCandidate(c, normalizedQuery))
          : g.items,
      }))
      .filter((g) => g.items.length > 0);
  }, [groups, normalizedQuery]);

  const flat = useMemo(() => matched.flatMap((g) => g.items), [matched]);
  // Avatars for what is on screen only — the batch hooks key off the list they
  // are given, so a narrowed query doesn't churn the whole fleet.
  const agentAvatarUrls = useAgentAvatarObjectUrls(
    useMemo(() => flat.flatMap((c) => (c.kind === "agent" ? [c.agent] : [])), [flat]),
  );
  const humanAvatarUrls = useUserAvatarObjectUrls(
    useMemo(() => flat.flatMap((c) => (c.kind === "human" ? [c.user] : [])), [flat]),
  );

  // Keep the highlight in range as the list narrows.
  useEffect(() => {
    if (active >= flat.length) setActive(0);
  }, [flat.length, active]);

  // Keep the highlighted row inside the scroll area.
  useEffect(() => {
    if (!listOpen) return;
    listRef.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active, listOpen]);

  const pick = (candidate: MemberCandidate): void => {
    onPick(candidate);
    setQuery("");
    onQueryChange?.("");
    setActive(0);
    if (!inline) setOpen(false);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>): void => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (!inline && !open) {
        setOpen(true);
        return;
      }
      if (flat.length > 0) setActive((i) => (i + 1) % flat.length);
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      if (flat.length > 0) setActive((i) => (i - 1 + flat.length) % flat.length);
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      const candidate = flat[active];
      if (candidate) pick(candidate);
      return;
    }
    // Floating swallows the first Escape to close its own list before the host
    // popover; inline has no list to close, so Escape falls through to the host.
    if (e.key === "Escape" && !inline && open) {
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    }
  };

  let flatIndex = -1;
  const rows =
    flat.length === 0 ? (
      loading ? (
        <InlineLoading label={t("workspaces.loadingMembers")} />
      ) : (
        <div className="px-2 py-2 text-center text-xs italic text-text-muted">
          {normalizedQuery ? t("workspaces.noMatches") : emptyHint}
        </div>
      )
    ) : (
      matched.map((group) => (
        <div key={group.label}>
          <div className="px-2 py-1 text-(length:--text-badge) font-medium uppercase tracking-wider text-text-muted">
            {group.label}
          </div>
          {group.items.map((candidate) => {
            flatIndex += 1;
            const index = flatIndex;
            const displayName = candidateDisplayName(candidate);
            const handle = candidateHandle(candidate);
            return (
              <button
                key={candidateKey(candidate)}
                type="button"
                role="option"
                aria-selected={index === active}
                data-active={index === active ? "true" : undefined}
                disabled={rowsDisabled}
                // preventDefault so the input never blurs (which would close a
                // floating list) before this click lands.
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => setActive(index)}
                onClick={() => pick(candidate)}
                className={cn(
                  "flex w-full items-center gap-2 rounded-(--select-item-radius) px-2 py-1.5 text-left disabled:opacity-60",
                  index === active ? "bg-(--bg-hover)" : "",
                )}
              >
                {candidate.kind === "agent" ? (
                  <AgentAvatar
                    runtime={candidate.agent.runtime}
                    size={AVATAR_PX.sm}
                    avatarObjectUrl={agentAvatarUrls.get(candidate.agent.id) ?? null}
                    alt={displayName}
                  />
                ) : (
                  <UserAvatar
                    size={AVATAR_PX.sm}
                    avatarObjectUrl={humanAvatarUrls.get(candidate.user.id) ?? null}
                    alt={displayName}
                  />
                )}
                {/* Name budget: the handle is capped and `shrink-0`, so an
                    overlong display name truncates first instead of squeezing
                    the handle away. */}
                <span className="flex min-w-0 flex-1 items-baseline gap-1.5">
                  <span
                    className="truncate text-sm font-medium text-text-primary"
                    title={displayName}
                  >
                    {displayName}
                  </span>
                  <span className="max-w-[45%] shrink-0 truncate font-mono text-(length:--text-meta) text-text-muted">
                    @{handle}
                  </span>
                </span>
              </button>
            );
          })}
        </div>
      ))
    );

  return (
    <div className={inline ? "flex flex-col gap-2" : "relative"}>
      <label data-input="" className={cn(SEARCH_FIELD_FRAME, disabled ? "opacity-60" : "")}>
        <SearchIcon className="size-3.5 shrink-0 text-text-muted" aria-hidden />
        <input
          id={inputId}
          type="text"
          role="combobox"
          aria-expanded={listOpen}
          aria-label={ariaLabel}
          aria-autocomplete="list"
          value={query}
          disabled={disabled}
          autoFocus={autoFocus}
          placeholder={placeholder}
          onChange={(e) => {
            setQuery(e.target.value);
            onQueryChange?.(e.target.value);
            setActive(0);
            if (!inline) setOpen(true);
          }}
          onFocus={() => {
            if (!inline) setOpen(true);
          }}
          // Floating: the list is in the same layer, so a row click keeps focus
          // (rows preventDefault on mousedown); blur therefore only fires when
          // focus really leaves the picker.
          onBlur={() => {
            if (!inline) setOpen(false);
          }}
          onKeyDown={onKeyDown}
          className="w-full min-w-0 bg-transparent text-sm text-text-primary placeholder:text-text-muted focus:outline-none"
        />
      </label>

      {inline ? (
        <div ref={listRef} role="listbox" className="flex max-h-[40vh] flex-col overflow-y-auto">
          {rows}
        </div>
      ) : open ? (
        <div
          ref={listRef}
          role="listbox"
          {...dropdownPanelProps(
            "poster",
            "absolute left-0 right-0 top-[calc(100%+4px)] z-[var(--z-raised)] max-h-[220px] overflow-y-auto p-1",
          )}
        >
          {rows}
        </div>
      ) : null}

      {footer}
    </div>
  );
}
