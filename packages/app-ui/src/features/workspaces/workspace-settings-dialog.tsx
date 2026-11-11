import { useEffect, useState, type FormEvent } from "react";
import {
  ArchiveIcon,
  GeneralSettingsIcon,
  LeaveIcon,
  MembersIcon,
  ShieldCheckIcon,
  UnarchiveIcon,
} from "@/icons";
import type { AgentCapability } from "@silverhand/shared";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input, Label } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { ConfirmButton } from "@/components/ui/confirm-button";
import { cn } from "@/lib/cn";
import { SidebarRow } from "@/components/layout/sidebar-shared";
import { nameFormatMessage } from "@/features/identity/use-name-availability";
import { AgentPermissionsPanel } from "./agent-permissions-panel";
import { WorkspaceMembersPanel } from "./manage-members-popover";
import { HumanRolesPanel } from "./human-roles-panel";
import {
  useArchiveWorkspace,
  useDeleteWorkspace,
  useLeaveWorkspace,
  useRenameWorkspace,
  useWorkspaceMemberLists,
} from "./hooks";
import {
  canLeaveWorkspace,
  canManageWorkspaceLifecycle,
  canRenameWorkspace,
} from "./workspace-permissions";
import type { WorkspaceCapabilities } from "./workspace-capabilities";
import { t } from "@/lib/i18n";

type SettingsSection = "general" | "members" | "agents";
type PermissionTab = "agents" | "human";

const permissionTabs = (): { id: PermissionTab; label: string }[] => [
  { id: "agents", label: t("workspaces.agents2") },
  { id: "human", label: t("workspaces.humans2") },
];

interface WorkspaceSettingsDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  workspaceId: string | null;
  workspaceName: string | null;
  /** What the container HAS, not what it is (#task:2066): the Permission
   *  section, the rename form and the lifecycle / leave blocks each key on one
   *  of these facts. Derived by `workspaceCapabilities` — this dialog never sees
   *  a workspace kind. */
  capabilities: WorkspaceCapabilities;
  /** epoch ms when archived; null for active workspaces. */
  archivedAt: number | null;
  onDeleted?: () => void;
  /** Deep-link: open straight to a section. Defaults to 'general'. The
   *  unified "open settings → Agent 权限" entry (channel jump today, future
   *  capability_denied notification) passes 'agents' + a focus agent. */
  initialSection?: SettingsSection;
  focusAgentId?: string | null;
  highlightCapability?: AgentCapability | null;
}

const navItems = (): { id: SettingsSection; label: string; icon: typeof GeneralSettingsIcon }[] => [
  { id: "general", label: t("workspaces.general"), icon: GeneralSettingsIcon },
  { id: "members", label: t("workspaces.members"), icon: MembersIcon },
  { id: "agents", label: t("workspaces.permission"), icon: ShieldCheckIcon },
];

export function WorkspaceSettingsDialog({
  open,
  onOpenChange,
  workspaceId,
  workspaceName,
  capabilities,
  archivedAt,
  onDeleted,
  initialSection = "general",
  focusAgentId = null,
  highlightCapability = null,
}: WorkspaceSettingsDialogProps): JSX.Element {
  const [section, setSection] = useState<SettingsSection>(initialSection);
  // Sub-tab within the Permission section. Agent capabilities and human
  // roles are orthogonal permission models, so they live as sibling tabs
  // rather than one merged list. A deep-link that targets an agent
  // (focusAgentId) implies the Agents tab.
  const [permTab, setPermTab] = useState<PermissionTab>("agents");
  const [name, setName] = useState("");
  const [renameError, setRenameError] = useState<string | null>(null);
  const renameWorkspace = useRenameWorkspace();
  const deleteWorkspace = useDeleteWorkspace();
  const leaveWorkspace = useLeaveWorkspace();
  const archiveWorkspace = useArchiveWorkspace();

  // Governance affordances are driven by the server-authoritative viewer_role
  // (the SAME source the Permission panels consume), never guessed from the
  // owner id or local identity. Fail-closed: while the members query is
  // unresolved `data` is undefined → role null → no write control renders, so
  // the UI never flashes a writable control it then retracts (#task:1772). The
  // backend gate stays the real boundary; this only decides what to show.
  const membersQuery = useWorkspaceMemberLists(workspaceId);
  const viewerRole = membersQuery.data?.viewerRole ?? null;
  const canRename = canRenameWorkspace(viewerRole);
  const canManageLifecycle = canManageWorkspaceLifecycle(viewerRole);
  const canLeave = canLeaveWorkspace(viewerRole);

  const { nameSettable, hasMembers, hasLifecycle } = capabilities;
  const isArchived = archivedAt !== null;
  // A container with no member set has no per-member permissions to show, so the
  // Permission section is not offered — and a deep-link that asks for it lands
  // on General rather than an empty panel.
  const visibleNavItems = hasMembers
    ? navItems()
    : navItems().filter((item) => item.id === "general");
  const activeSection = !hasMembers && section !== "general" ? "general" : section;

  // Reset every time the dialog reopens for a (potentially different)
  // workspace.
  useEffect(() => {
    if (open) {
      setSection(hasMembers ? initialSection : "general");
      setPermTab("agents");
      setName(workspaceName ?? "");
      setRenameError(null);
    }
  }, [open, workspaceId, workspaceName, initialSection, hasMembers]);

  const handleRename = async (e: FormEvent) => {
    e.preventDefault();
    setRenameError(null);
    if (!workspaceId) return;
    const trimmed = name.trim();
    if (!trimmed) {
      setRenameError(t("workspaces.pleaseEnterAProject"));
      return;
    }
    // ONE format rule for both doors (#task:1827): the shared
    // `WorkspaceNameSchema` verdict, through the same copy the create dialog
    // shows. Rename previously kept a local `/^[a-zA-Z0-9_-]+$/`, so a
    // workspace create had just admitted as `设计 评审` could not be renamed at
    // all. Uniqueness stays server-only — the PATCH answers 409.
    const formatMessage = nameFormatMessage("workspace", trimmed);
    if (formatMessage) {
      setRenameError(formatMessage);
      return;
    }
    if (trimmed === workspaceName) return;
    try {
      await renameWorkspace.mutateAsync({ id: workspaceId, name: trimmed });
    } catch (err) {
      setRenameError(String(err));
    }
  };

  const handleDelete = () => {
    if (!workspaceId || deleteWorkspace.isPending) return;
    deleteWorkspace.mutate(workspaceId, {
      onSuccess: () => {
        onOpenChange(false);
        onDeleted?.();
      },
    });
  };

  const handleLeave = () => {
    if (!workspaceId || leaveWorkspace.isPending) return;
    leaveWorkspace.mutate(workspaceId, {
      onSuccess: () => {
        // Same reactive bounce as Delete: the invalidated workspaces list drops
        // this row and the selection effect navigates away. The dialog only
        // needs to close.
        onOpenChange(false);
      },
    });
  };

  const handleToggleArchive = () => {
    if (!workspaceId || archiveWorkspace.isPending) return;
    archiveWorkspace.mutate(
      { id: workspaceId, archived: !isArchived },
      {
        onSuccess: () => {
          // Close so the user sees the workspace either disappear into
          // the Archived group or pop back into the active list.
          onOpenChange(false);
        },
      },
    );
  };

  const renameDirty = name.trim() !== (workspaceName ?? "");
  const renameDisabled = !nameSettable || renameWorkspace.isPending || !renameDirty || !workspaceId;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-4xl">
        <DialogHeader>
          <DialogTitle className="flex items-baseline gap-2">
            {t("workspaces.projectSettings")}
            {workspaceName ? (
              <span className="min-w-0 truncate text-sm font-normal text-text-muted">
                {workspaceName}
              </span>
            ) : null}
          </DialogTitle>
        </DialogHeader>

        {/* The only dialog body with a FIXED height, so it is the only one that
            can outgrow a top-anchored shell (#task:1829): a centred shell used
            to split the overflow across both edges, a top-anchored one puts all
            of it past the bottom — taking the footer with it. Capping against
            the viewport keeps 32rem as the intent and lets a short window
            shrink the body instead of hiding it. 65vh, not 70: the shell costs
            15vh above plus ~92px of chrome, so 65 keeps the footer on screen
            down to a ~460px-tall window — below that nothing in this app is
            usable anyway. */}
        <div data-project-settings-body className="flex h-[min(32rem,65vh)] gap-5">
          <nav data-project-settings-nav className="w-40 shrink-0">
            <ul className="flex flex-col gap-0.5">
              {visibleNavItems.map((item) => {
                const active = activeSection === item.id;
                const Icon = item.icon;
                return (
                  <SidebarRow
                    key={item.id}
                    icon={<Icon className="size-3.5 shrink-0" />}
                    title={item.label}
                    selected={active}
                    aria-current={active ? "page" : undefined}
                    onClick={() => setSection(item.id)}
                  />
                );
              })}
            </ul>
          </nav>

          <div className="scrollbar-hide min-w-0 flex-1 overflow-y-auto pr-1">
            {activeSection === "general" ? (
              <GeneralSection
                key={workspaceId}
                nameSettable={nameSettable}
                hasLifecycle={hasLifecycle}
                isArchived={isArchived}
                canRename={canRename}
                canManageLifecycle={canManageLifecycle}
                canLeave={canLeave}
                name={name}
                onNameChange={setName}
                onRename={handleRename}
                renameError={renameError}
                renameDisabled={renameDisabled}
                renamePending={renameWorkspace.isPending}
                workspaceId={workspaceId}
                onToggleArchive={handleToggleArchive}
                archivePending={archiveWorkspace.isPending}
                onDelete={handleDelete}
                deletePending={deleteWorkspace.isPending}
                onLeave={handleLeave}
                leavePending={leaveWorkspace.isPending}
              />
            ) : activeSection === "members" && workspaceId ? (
              <WorkspaceMembersPanel
                key={workspaceId}
                workspaceId={workspaceId}
                onNavigate={() => onOpenChange(false)}
              />
            ) : (
              <div className="flex flex-col gap-4">
                {/* Permission sub-tabs: agent capabilities vs human roles.
                    Horizontal segmented control pinned above the active
                    panel. */}
                <div
                  role="tablist"
                  aria-label={t("workspaces.permissionTarget")}
                  className="flex items-center gap-1 border-b-(length:--control-border-width) border-border-subtle"
                >
                  {permissionTabs().map((tab) => {
                    const active = permTab === tab.id;
                    return (
                      <button
                        key={tab.id}
                        type="button"
                        role="tab"
                        aria-selected={active}
                        onClick={() => setPermTab(tab.id)}
                        className={cn(
                          "-mb-px border-b-2 px-3 py-1.5 text-sm transition-colors",
                          active
                            ? "border-border-focus font-medium text-text-primary"
                            : "border-transparent text-text-secondary hover:text-text-primary",
                        )}
                      >
                        {tab.label}
                      </button>
                    );
                  })}
                </div>

                {permTab === "agents" ? (
                  <AgentPermissionsPanel
                    workspaceId={workspaceId}
                    focusAgentId={focusAgentId}
                    highlightCapability={highlightCapability}
                  />
                ) : (
                  <HumanRolesPanel workspaceId={workspaceId} />
                )}
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

interface GeneralSectionProps {
  /** Structural: the name is the user's to set (false = derived from the owning
   *  agent). Orthogonal to `canRename`, which is the viewer's role. */
  nameSettable: boolean;
  /** Structural: the container has its own archive / delete / leave doors. */
  hasLifecycle: boolean;
  isArchived: boolean;
  /** Viewer may rename (owner|admin). When false, Name is shown read-only. */
  canRename: boolean;
  /** Viewer may archive/delete (owner). When false, both blocks are hidden. */
  canManageLifecycle: boolean;
  name: string;
  onNameChange: (v: string) => void;
  onRename: (e: FormEvent) => void;
  renameError: string | null;
  renameDisabled: boolean;
  renamePending: boolean;
  workspaceId: string | null;
  onToggleArchive: () => void;
  archivePending: boolean;
  onDelete: () => void;
  deletePending: boolean;
  canLeave: boolean;
  onLeave: () => void;
  leavePending: boolean;
}

function GeneralSection({
  nameSettable,
  hasLifecycle,
  isArchived,
  canRename,
  canManageLifecycle,
  name,
  onNameChange,
  onRename,
  renameError,
  renameDisabled,
  renamePending,
  workspaceId,
  onToggleArchive,
  archivePending,
  onDelete,
  deletePending,
  canLeave,
  onLeave,
  leavePending,
}: GeneralSectionProps): JSX.Element {
  // A derived name is immutable (it follows its agent); a settable one is
  // editable only for owner|admin. Everyone else sees the name read-only — the
  // value is still worth showing, but there is no Save affordance that would
  // 403.
  const nameEditable = nameSettable && canRename;

  return (
    <div className="flex flex-col gap-4">
      {!nameSettable ? (
        <p className="text-xs text-text-muted">
          {/* The one producer of a non-settable name today is an agent space, so
           * this says WHY it is fixed: the name IS its agent's handle, which the
           * agent cannot change either (#task:2044 — the space reads as its
           * agent's name). A second producer must carry its own reason. */}
          {t("workspaces.anAgentSpaceTakes")}
        </p>
      ) : canRename ? (
        <p className="text-xs text-text-muted">{t("workspaces.renamingNotifiesOnlineAgents")}</p>
      ) : null}

      {nameEditable ? (
        <form onSubmit={onRename} className="flex flex-col gap-1.5">
          <Label htmlFor="workspace-settings-name">{t("workspaces.name")}</Label>
          <div className="flex gap-2">
            <Input
              id="workspace-settings-name"
              value={name}
              onChange={(e) => onNameChange(e.target.value)}
              placeholder={t("workspaces.myFrontend")}
              autoFocus
              className="transition-colors hover:border-(--border-control-hover)"
            />
            <Button type="submit" variant="primary" disabled={renameDisabled}>
              {renamePending ? t("workspaces.saving") : t("common.save")}
            </Button>
          </div>
          {renameError ? <div className="text-xs text-error">{renameError}</div> : null}
        </form>
      ) : (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="workspace-settings-name">{t("workspaces.name")}</Label>
          <Input
            id="workspace-settings-name"
            value={name}
            readOnly
            onFocus={(e) => e.currentTarget.select()}
          />
        </div>
      )}

      {hasLifecycle && canManageLifecycle ? (
        <>
          {/* Archive + Delete are owner-only administrative ACTIONS (gated on
              canManageLifecycle). Unlike Name, they carry no read value for a
              viewer who can't perform them — a disabled button + "owner required"
              caption would be pure noise and a false affordance — so they are
              hidden entirely rather than shown disabled (#task:1772).

              Administrative blocks are static panels, not controls: the only
              affordance is the Button inside, so neither this one nor the
              Delete block below reacts to hover (shiki 2026-07-27 — Archive
              used to light up its border while Delete stayed inert). A hover
              frame on a whole panel promises a click the panel does not
              honour, and the Delete block's `border-error/30` is identity
              (danger tint) that hover must not recolour anyway. */}
          <div
            data-project-settings-panel
            className="flex items-center justify-between gap-3 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-elevated/40 p-3"
          >
            <div className="flex flex-col gap-0.5">
              <span className="text-sm font-medium text-text-primary">
                {isArchived ? t("common.archived") : t("workspaces.archiveProject")}
              </span>
              <span className="text-xs text-text-muted">
                {isArchived
                  ? t("workspaces.collapsedIntoTheSidebar")
                  : t("workspaces.collapseIntoTheSidebar")}
              </span>
            </div>
            <Button
              type="button"
              variant="neutral"
              onClick={onToggleArchive}
              disabled={!workspaceId || archivePending}
            >
              {isArchived ? (
                <>
                  <UnarchiveIcon className="mr-1.5 size-3.5" />
                  {archivePending ? t("workspaces.working") : t("workspaces.unarchive")}
                </>
              ) : (
                <>
                  <ArchiveIcon className="mr-1.5 size-3.5" />
                  {archivePending ? t("workspaces.working") : t("workspaces.archive")}
                </>
              )}
            </Button>
          </div>

          <div
            data-project-settings-panel
            className="flex items-center justify-between gap-3 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-elevated/40 p-3"
          >
            <div className="flex flex-col gap-0.5">
              <span className="text-sm font-medium text-text-primary">
                {t("workspaces.deleteProject")}
              </span>
              <span className="text-xs text-text-muted">
                {t("workspaces.allChannelsThreadsAnd")}
              </span>
            </div>
            <ConfirmButton
              onConfirm={onDelete}
              disabled={!workspaceId}
              pending={deletePending}
              pendingLabel={t("workspaces.deleting")}
              confirmLabel={t("workspaces.deletePermanently")}
            >
              {t("workspaces.delete")}
            </ConfirmButton>
          </div>
        </>
      ) : null}

      {/* Leave is the member/admin counterpart of the owner's Delete — the two
          are complementary over any role, so exactly one shows. It destroys no
          content, so its commit stays ink rather than Delete's red. Access is not freely restored (rejoining re-seeds no channel
          membership), but the caption stays terse per shiki; do NOT reintroduce
          a "re-invite restores access" line — it is false for channels. Owner
          never reaches here (canLeave is false for owner). */}
      {hasLifecycle && canLeave ? (
        <div
          data-project-settings-panel
          className="flex items-center justify-between gap-3 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-elevated/40 p-3"
        >
          <div className="flex flex-col gap-0.5">
            <span className="text-sm font-medium text-text-primary">
              {t("workspaces.leaveProject")}
            </span>
            <span className="text-xs text-text-muted">{t("workspaces.youLeaveThisProject")}</span>
          </div>
          <ConfirmButton
            onConfirm={onLeave}
            variant="neutral"
            disabled={!workspaceId}
            pending={leavePending}
            pendingLabel={t("workspaces.leaving")}
            confirmLabel={t("workspaces.leave")}
            confirmIcon={<LeaveIcon className="size-3.5" />}
          >
            <LeaveIcon className="size-3.5" />
            {t("workspaces.leave")}
          </ConfirmButton>
        </div>
      ) : null}
    </div>
  );
}
