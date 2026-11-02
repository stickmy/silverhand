import { SkeletonBlock, SkeletonRegion } from "@/components/layout/skeleton";
import { useState } from "react";
import * as Select from "@radix-ui/react-select";
import { EmptyIcon, ChevronDownIcon, ErrorIcon } from "@/icons";
import { PanelEmpty, PanelError } from "./permissions-panel-state";
import { toast } from "@/lib/toast";
import type { AgentCapability, AgentDto } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import { getCurrentUserId } from "@/features/session/current-user";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { cn } from "@/lib/cn";
import { buildCapabilityGroups, CAPABILITY_COUNT } from "./capability-meta";
import { canEditAgentCapabilities } from "./agent-permissions";
import { useAgentCapabilities, useUpdateAgentCapabilities, useWorkspaceMemberLists } from "./hooks";
import { t } from "@/lib/i18n";

const GROUPS = buildCapabilityGroups();

/** Stable empty reference so the avatar batch hook doesn't re-fetch on
 *  every render while members are still loading. */
const EMPTY_AGENTS: AgentDto[] = [];

function setEq(a: Set<string>, b: Set<string>): boolean {
  if (a.size !== b.size) return false;
  for (const x of a) if (!b.has(x)) return false;
  return true;
}

/** Turn a PUT failure into an operator-facing message. The backend gates
 *  edits on the human role (403) and rejects unknown buckets via the enum
 *  (400) — both must be visible even though the front-end also gates. */
function describeSaveError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.kind === "backend_unreachable") return t("workspaces.saveFailedCannotReach");
    if (err.status === 403) return t("workspaces.youDontHaveEdit");
    if (err.status === 400) return t("workspaces.saveFailedSubmittedAn");
    return t("workspaces.saveFailedStatus", { status: err.status });
  }
  return t("workspaces.saveFailed");
}

export interface AgentPermissionsPanelProps {
  workspaceId: string | null;
  /** Deep-link seam (Alice review note 2): which agent to preselect and
   *  which capability to highlight. The panel only CONSUMES these — it never
   *  raises the capability_denied feedback itself (that lives in the
   *  notification layer). */
  focusAgentId?: string | null;
  highlightCapability?: AgentCapability | null;
}

export function AgentPermissionsPanel({
  workspaceId,
  focusAgentId,
  highlightCapability,
}: AgentPermissionsPanelProps): JSX.Element {
  const membersQuery = useWorkspaceMemberLists(workspaceId);
  const agents = membersQuery.data?.agents ?? EMPTY_AGENTS;
  const viewerRole = membersQuery.data?.viewerRole;

  const [picked, setPicked] = useState<string | null>(focusAgentId ?? null);
  const avatarUrls = useAgentAvatarObjectUrls(agents);
  // Resolve to a valid selection once members load; default to the first.
  const selectedId =
    picked && agents.some((a) => a.id === picked) ? picked : (agents[0]?.id ?? null);
  const selectedAgent = agents.find((a) => a.id === selectedId) ?? null;

  // Per-agent edit gate (#task:1778): a workspace governor (owner|admin) edits
  // any agent; everyone else edits ONLY the agent they created. Derived from
  // viewer_role + THIS agent's created_by_user_id, fail-closed while either is
  // unresolved (see canEditAgentCapabilities). Backend #task:1777 is the
  // boundary; this only mirrors it so no read-only viewer sees a Save that 403s.
  const canEdit = selectedAgent
    ? canEditAgentCapabilities({
        viewerRole,
        viewerUserId: getCurrentUserId(),
        agentCreatedByUserId: selectedAgent.created_by_user_id,
      })
    : false;

  if (membersQuery.isLoading) {
    return <PanelSkeleton />;
  }
  if (membersQuery.isError) {
    return <PanelError onRetry={() => void membersQuery.refetch()} />;
  }
  if (agents.length === 0) {
    return (
      <PanelEmpty icon={<EmptyIcon className="size-6" />}>
        {t("workspaces.noAgentsInThis")}
      </PanelEmpty>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <AgentSelect
        agents={agents}
        value={selectedId}
        avatarUrls={avatarUrls}
        onChange={setPicked}
      />

      {selectedAgent ? (
        <>
          {/* Read-only notice is per selected agent (the gate is per-agent now):
              it explains why THIS agent's editor is read-only, so it sits with
              the editor below the picker, not above it. */}
          {!canEdit ? (
            <div className="flex items-start gap-2 rounded-(--control-radius) border-(length:--control-border-width) border-border-subtle bg-bg-elevated/40 p-2.5 text-xs text-text-muted">
              <ErrorIcon className="mt-0.5 size-3.5 shrink-0" />
              <span>{t("workspaces.permissionsReadOnly")}</span>
            </div>
          ) : null}
          <AgentCapabilityEditor
            key={selectedAgent.id}
            workspaceId={workspaceId!}
            agent={selectedAgent}
            canEdit={canEdit}
            highlightCapability={highlightCapability ?? null}
          />
        </>
      ) : null}
    </div>
  );
}

function AgentSelect({
  agents,
  value,
  avatarUrls,
  onChange,
}: {
  agents: AgentDto[];
  value: string | null;
  avatarUrls: Map<string, string | null>;
  onChange: (id: string) => void;
}): JSX.Element {
  const selected = agents.find((a) => a.id === value) ?? null;
  return (
    <Select.Root value={value ?? undefined} onValueChange={onChange}>
      <Select.Trigger
        className={cn(
          "flex h-9 w-full items-center justify-between gap-2 rounded-(--control-radius) bg-bg-floating px-3 text-sm",
          "border-(length:--control-border-width) border-border transition-colors",
          "hover:border-(--border-control-hover)",
          "data-[state=open]:border-border-focus focus-visible:outline-none focus-visible:border-border-focus",
        )}
      >
        {selected ? (
          <span className="flex min-w-0 items-center gap-2">
            <AgentAvatar
              runtime={selected.runtime}
              size={AVATAR_PX.sm}
              avatarObjectUrl={avatarUrls.get(selected.id) ?? null}
            />
            <span className="truncate font-medium text-text-primary">
              {selected.display_name || selected.name}
            </span>
            <span className="truncate text-text-muted">@{selected.name}</span>
          </span>
        ) : (
          <span className="text-text-muted">{t("workspaces.selectAnAgent")}</span>
        )}
        <Select.Icon>
          <ChevronDownIcon className="size-3.5 shrink-0 text-text-muted" />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content
          position="popper"
          sideOffset={4}
          className="z-(--z-dialog) min-w-(--radix-select-trigger-width) rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-floating p-1 text-sm shadow-popover"
        >
          <Select.Viewport>
            {agents.map((agent) => (
              <Select.Item
                key={agent.id}
                value={agent.id}
                className="flex cursor-default items-center gap-2 rounded-(--control-radius) px-2 py-1.5 text-text-primary outline-none data-highlighted:bg-bg-hover"
              >
                <AgentAvatar
                  runtime={agent.runtime}
                  size={AVATAR_PX.sm}
                  avatarObjectUrl={avatarUrls.get(agent.id) ?? null}
                />
                <Select.ItemText>
                  <span className="font-medium text-text-primary">
                    {agent.display_name || agent.name}
                  </span>
                  <span className="ml-1.5 text-text-muted">@{agent.name}</span>
                </Select.ItemText>
              </Select.Item>
            ))}
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

interface AgentCapabilityEditorProps {
  workspaceId: string;
  agent: AgentDto;
  canEdit: boolean;
  highlightCapability: AgentCapability | null;
}

function AgentCapabilityEditor({
  workspaceId,
  agent,
  canEdit,
  highlightCapability,
}: AgentCapabilityEditorProps): JSX.Element {
  const capsQuery = useAgentCapabilities(workspaceId, agent.id);
  const update = useUpdateAgentCapabilities(workspaceId);

  const server = new Set<AgentCapability>(capsQuery.data?.capabilities ?? []);
  // `null` draft = follow server. A non-null draft is the in-progress edit
  // for the replace-set save; cleared on save success / reset / refetch.
  const [draft, setDraft] = useState<Set<AgentCapability> | null>(null);
  const effective = draft ?? server;
  const dirty = draft !== null && !setEq(draft, server);
  const grantedCount = server.size;

  const toggle = (cap: AgentCapability, checked: boolean) => {
    if (!canEdit) return;
    const next = new Set(effective);
    if (checked) next.add(cap);
    else next.delete(cap);
    setDraft(next);
  };

  const onSave = async () => {
    try {
      // replace-set: send the FULL target set, not a delta.
      await update.mutateAsync({
        agentId: agent.id,
        capabilities: [...effective],
      });
      setDraft(null);
      toast.success(t("workspaces.permissionsUpdated", { name: agent.display_name || agent.name }));
    } catch (err) {
      // Roll the optimistic draft back to server truth and refetch.
      setDraft(null);
      void capsQuery.refetch();
      toast.error(describeSaveError(err));
    }
  };

  if (capsQuery.isLoading) {
    return (
      <SkeletonRegion
        label={t("workspaces.loadingPermissions")}
        className="rounded-(--control-radius) border-(length:--control-border-width) border-border p-4"
      >
        <div className="columns-2 gap-x-6">
          {GROUPS.map((group) => (
            <div key={group.id} className="mb-4 break-inside-avoid space-y-1.5">
              <div className="text-(length:--text-meta) font-semibold uppercase text-text-muted">
                {group.label}
              </div>
              {group.items.map((item) => (
                <div key={item.key} className="flex gap-2 px-1.5 py-1">
                  <div className="w-3.5 shrink-0" />
                  <div className="flex-1 space-y-1.5">
                    <SkeletonBlock className="h-3 w-3/4" />
                    <SkeletonBlock className="h-2.5 w-full" />
                  </div>
                </div>
              ))}
            </div>
          ))}
        </div>
      </SkeletonRegion>
    );
  }
  if (capsQuery.isError) {
    return (
      <div className="flex items-center gap-2 text-xs text-error">
        {t("workspaces.failedToLoadPermissions")}
        <Button variant="ghost" size="sm" onClick={() => void capsQuery.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-floating p-3.5">
      <div className="flex items-center justify-end">
        <span className="text-xs tabular-nums text-text-muted">
          {t("workspaces.grantedCount", { granted: grantedCount, total: CAPABILITY_COUNT })}
        </span>
      </div>

      {/* Two-column flow: capability groups are variable-height blocks, so
          CSS multi-column balances them and halves the scroll height in the
          wide (max-w-4xl) dialog. `break-inside-avoid` keeps a group whole;
          multicol can't use flex `gap`, so groups carry their own `mb-4`. */}
      <div className="columns-2 gap-x-6">
        {GROUPS.map((group) => (
          <div key={group.id} className="mb-4 flex break-inside-avoid flex-col gap-1.5 last:mb-0">
            <div className="text-(length:--text-meta) font-semibold uppercase tracking-wide text-text-muted">
              {group.label}
            </div>
            {group.items.map((item) => {
              const checked = effective.has(item.key);
              const highlight = highlightCapability === item.key;
              return (
                <div
                  key={item.key}
                  className={cn(
                    "flex items-start gap-2 rounded-(--control-radius) px-1.5 py-1",
                    highlight ? "[outline:2px_solid_var(--border-focus)] -outline-offset-2" : null,
                  )}
                >
                  <Checkbox
                    checked={checked}
                    disabled={!canEdit || update.isPending}
                    onCheckedChange={(c) => toggle(item.key, c)}
                    className="mt-0.5"
                    aria-label={item.label}
                  />
                  <span className="flex flex-col">
                    <span className="text-xs font-medium text-text-primary">{item.label}</span>
                    <span className="text-(length:--text-meta) leading-tight text-text-muted">
                      {item.hint}
                    </span>
                  </span>
                </div>
              );
            })}
          </div>
        ))}
      </div>

      {canEdit ? (
        // Sticky footer pinned to the scroll viewport bottom (the panel is
        // the `overflow-y-auto` ancestor) so Save stays reachable without
        // scrolling to the end of a long capability list. Negative margins
        // cancel the card padding so it spans full width and sits flush at
        // the card's bottom edge; the card bg covers rows scrolling behind.
        <div className="sticky bottom-0 -mx-3.5 -mb-3.5 mt-1 flex items-center gap-2 border-t-(length:--control-border-width) border-border bg-bg-floating px-3.5 py-3">
          <Button
            size="sm"
            variant="primary"
            onClick={() => void onSave()}
            disabled={!dirty || update.isPending}
          >
            {update.isPending ? t("workspaces.saving") : t("common.save")}
          </Button>
          <Button
            variant="cancel"
            size="sm"
            onClick={() => setDraft(null)}
            disabled={!dirty || update.isPending}
          >
            {t("common.cancel")}
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function PanelSkeleton(): JSX.Element {
  return (
    <div className="flex flex-col gap-3">
      <div className="h-9 animate-pulse rounded-(--control-radius) border border-border-subtle bg-bg-elevated/40" />
      <div className="h-64 animate-pulse rounded-(--control-radius) border border-border-subtle bg-bg-elevated/40" />
    </div>
  );
}
