import { CloseIcon } from "@/icons";
import { useMemo } from "react";
import type { AgentDto, UserDto } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { t, useLocale } from "@/lib/i18n";
import { MemberCandidatePicker } from "./member-candidate-picker";
import type { MemberCandidateGroup } from "./member-search";

interface AgentPickerProps {
  candidates: AgentDto[];
  value: string[];
  onChange: (ids: string[]) => void;
  placeholder?: string;
  emptyText?: string;
  allSelectedText?: string;
  selectId?: string;
}

export type MemberPickerCandidate =
  | { kind: "agent"; agent: AgentDto }
  | { kind: "human"; user: UserDto };

interface MemberPickerProps {
  candidates: MemberPickerCandidate[];
  value: string[];
  onChange: (values: string[]) => void;
  placeholder?: string;
  emptyText?: string;
  allSelectedText?: string;
  selectId?: string;
}

/**
 * Shared member picker. The add-member search field (the same
 * `MemberCandidatePicker` the rosters use, so typing narrows the pool)
 * stays anchored at the bottom; selected members render ABOVE it as compact wrapping
 * tags (avatar + display_name + monospace `@handle` + tiny remove X)
 * rather than full-width rows — picked members are confirmed identity
 * chips, not "directory entries you can re-inspect". Per shiki
 * #task:888: "选择过的不要出现在下方,而是在 Input 上方出现,但是不要
 * 一个 member 占一行,而是一个 tag,显示头像、displayname, handle".
 *
 * Tag pills wrap on overflow; the trigger sits below them, so the
 * member-management surface reads as "what's in / how to add more"
 * (top-down: state, then action) instead of "how to add / what's in"
 * which inverts the natural editing flow.
 *
 * Callers are responsible for filtering candidates (e.g. excluding the
 * viewer because the backend auto-includes the workspace/channel
 * creator as a member); the picker stays a pure presentation primitive.
 * `AgentPicker` remains as the agent-only compatibility wrapper for
 * older surfaces; `MemberPicker` accepts agents and humans for
 * workspace/channel creation.
 */
export function AgentPicker({
  candidates,
  value,
  onChange,
  placeholder = t("workspaces.addAMember"),
  emptyText = t("workspaces.noMembersAvailable"),
  allSelectedText = t("workspaces.everyoneIsAlreadyAdded"),
  selectId,
}: AgentPickerProps): JSX.Element {
  const memberCandidates = useMemo<MemberPickerCandidate[]>(
    () => candidates.map((agent) => ({ kind: "agent", agent })),
    [candidates],
  );
  const memberValue = useMemo(() => value.map((id) => `agent:${id}`), [value]);
  return (
    <MemberPicker
      candidates={memberCandidates}
      value={memberValue}
      onChange={(values) =>
        onChange(
          values
            .map((v) => decodeMemberKey(v))
            .filter((v): v is { kind: "agent"; id: string } => v?.kind === "agent")
            .map((v) => v.id),
        )
      }
      placeholder={placeholder}
      emptyText={emptyText}
      allSelectedText={allSelectedText}
      selectId={selectId}
    />
  );
}

export function MemberPicker({
  candidates,
  value,
  onChange,
  placeholder = t("workspaces.addAMember"),
  emptyText = t("workspaces.noMembersAvailable"),
  allSelectedText = t("workspaces.everyoneIsAlreadyAdded"),
  selectId,
}: MemberPickerProps): JSX.Element {
  const byKey = new Map(candidates.map((c) => [memberKey(c), c]));
  const selectedKeys = value.filter((id, index) => value.indexOf(id) === index);
  const selectedSet = new Set(selectedKeys);
  const locale = useLocale();
  const groups = useMemo<MemberCandidateGroup[]>(() => {
    const picked = new Set(value);
    const available = candidates.filter((c) => !picked.has(memberKey(c)));
    return [
      { label: t("workspaces.agents"), items: available.filter((c) => c.kind === "agent") },
      { label: t("workspaces.humans"), items: available.filter((c) => c.kind === "human") },
    ].filter((g) => g.items.length > 0);
  }, [locale, candidates, value]);

  // Resolve avatars for the whole candidate pool — selected rows and
  // dropdown items share the same map. The hook caches by
  // `(id, avatar_url)` so the same member rendered in both surfaces
  // only fetches once.
  const agentCandidates = useMemo(
    () => candidates.flatMap((c) => (c.kind === "agent" ? [c.agent] : [])),
    [candidates],
  );
  const humanCandidates = useMemo(
    () => candidates.flatMap((c) => (c.kind === "human" ? [c.user] : [])),
    [candidates],
  );
  const agentAvatars = useAgentAvatarObjectUrls(agentCandidates);
  const humanAvatars = useAgentAvatarObjectUrls(humanCandidates);

  const addMember = (id: string): void => {
    if (!id || selectedSet.has(id)) return;
    onChange([...selectedKeys, id]);
  };

  const removeMember = (id: string): void => {
    onChange(selectedKeys.filter((v) => v !== id));
  };

  return (
    <div className="flex flex-col gap-1.5">
      {selectedKeys.length > 0 ? (
        <ul className="flex flex-wrap gap-1.5" aria-label={t("workspaces.selectedMembers")}>
          {selectedKeys.map((id) => {
            const candidate = byKey.get(id);
            if (!candidate) {
              // Lost reference (candidates rebuilt without this id) —
              // render a minimal placeholder so the user can still
              // remove the orphan from `value`.
              return (
                <li key={id} className="flex h-6">
                  <SelectedTag
                    avatar={null}
                    displayName={id.slice(0, 8)}
                    handle={null}
                    onRemove={() => removeMember(id)}
                    ariaRemoveLabel={t("workspaces.removeNamed", { name: id.slice(0, 8) })}
                    muted
                  />
                </li>
              );
            }
            const displayName = memberDisplayName(candidate);
            const handle = memberHandle(candidate);
            const avatar =
              candidate.kind === "agent" ? (
                <AgentAvatar
                  runtime={candidate.agent.runtime}
                  // 14px avatar inside a 24px chip — see SelectedTag
                  // comment for rationale (round 9.1: "头像占的太满
                  // 了, 失去了空间感"). The smaller glyph leaves
                  // ~5px above/below so the avatar reads as a small
                  // identity icon rather than a tile.
                  size={AVATAR_PX.chip}
                  avatarObjectUrl={agentAvatars.get(candidate.agent.id) ?? null}
                  alt={displayName}
                />
              ) : (
                <UserAvatar
                  size={AVATAR_PX.chip}
                  avatarObjectUrl={humanAvatars.get(candidate.user.id) ?? null}
                  alt={displayName}
                />
              );
            return (
              <li key={id} className="flex h-6">
                <SelectedTag
                  avatar={avatar}
                  displayName={displayName}
                  handle={handle}
                  onRemove={() => removeMember(id)}
                  ariaRemoveLabel={t("workspaces.removeNamed", { name: `@${handle}` })}
                />
              </li>
            );
          })}
        </ul>
      ) : null}

      <MemberCandidatePicker
        mode="floating"
        inputId={selectId}
        groups={groups}
        onPick={(candidate) => addMember(memberKey(candidate))}
        disabled={candidates.length === 0}
        ariaLabel={t("workspaces.addMember")}
        emptyHint={candidates.length === 0 ? emptyText : allSelectedText}
        placeholder={placeholder}
      />
    </div>
  );
}

/**
 * Selected-member chip — compact pill with avatar + display_name +
 * mono `@handle` + inline remove X. Multiple chips wrap inside the
 * parent `flex-wrap` row; the chip itself is `inline-flex` so it
 * sizes to its content. Per shiki #task:888: NOT a full-width row —
 * picked members are confirmation chips, not directory entries.
 */
function SelectedTag({
  avatar,
  displayName,
  handle,
  onRemove,
  ariaRemoveLabel,
  muted = false,
}: {
  avatar: React.ReactNode | null;
  displayName: string;
  handle: string | null;
  onRemove: () => void;
  ariaRemoveLabel: string;
  muted?: boolean;
}): JSX.Element {
  // Chip frame is sized by the parent `<li>` (`flex h-6`); the chip
  // span uses `h-full` to fill it edge-to-edge so the chip background
  // never reveals a stripe of `<li>` above/below it. The avatar is
  // intentionally NOT stretched to the chip height — round 9.1 (the
  // user noted "头像占的太满了, 失去了空间感"): a 14px avatar in a
  // 24px chip leaves ~5px above/below + 6px on the left, so the
  // avatar reads as a glyph floating inside the chip rather than a
  // face-shaped tile filling the row. `leading-none` strips
  // text-xs's intrinsic line-height padding so the label visual
  // height matches the chip frame.
  // Per shiki followup on #task:888: "高度不统一,建议设置一个固定
  // 高度,而不是由内容决定,否则会受到头像影响" + "li 是 24px 了,但
  // 是内部的 span 没有撑满高度" — that's still respected: the
  // chip span itself is `h-full`; only the avatar is allowed to
  // be smaller than the frame.
  return (
    <span
      className={
        "inline-flex h-full max-w-full items-center gap-1.5 rounded-(--chip-radius) border border-border-subtle bg-bg-base pl-1.5 pr-1 text-xs leading-none " +
        (muted ? "text-text-muted" : "text-text-primary")
      }
    >
      {avatar ? <span className="inline-flex shrink-0 items-center">{avatar}</span> : null}
      <span className="flex min-w-0 items-baseline gap-1">
        <span className="truncate font-medium">{displayName}</span>
        {handle ? (
          <span className="truncate font-mono text-(length:--text-meta) text-text-muted">
            @{handle}
          </span>
        ) : null}
      </span>
      <button
        type="button"
        onClick={onRemove}
        aria-label={ariaRemoveLabel}
        title={t("workspaces.removeMember")}
        className="flex size-4 shrink-0 items-center justify-center rounded-(--chip-radius) text-text-muted transition-colors hover:text-text-primary focus-outline"
      >
        <CloseIcon className="size-3" aria-hidden />
      </button>
    </span>
  );
}

function memberKey(candidate: MemberPickerCandidate): string {
  return candidate.kind === "agent" ? `agent:${candidate.agent.id}` : `human:${candidate.user.id}`;
}

function decodeMemberKey(value: string): { kind: "agent" | "human"; id: string } | null {
  const sep = value.indexOf(":");
  if (sep <= 0) return null;
  const kind = value.slice(0, sep);
  const id = value.slice(sep + 1);
  if (kind !== "agent" && kind !== "human") return null;
  return { kind, id };
}

function memberDisplayName(candidate: MemberPickerCandidate): string {
  return candidate.kind === "agent"
    ? candidate.agent.display_name || candidate.agent.name
    : candidate.user.display_name || candidate.user.name;
}

function memberHandle(candidate: MemberPickerCandidate): string {
  return candidate.kind === "agent" ? candidate.agent.name : candidate.user.name;
}
