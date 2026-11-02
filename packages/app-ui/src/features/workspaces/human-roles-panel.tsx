import * as Select from "@radix-ui/react-select";
import { CheckIcon, ChevronDownIcon, ErrorIcon, EmptyIcon } from "@/icons";
import { PanelEmpty, PanelError } from "./permissions-panel-state";
import { toast } from "@/lib/toast";
import type { WorkspaceHumanMemberDto, WorkspaceRole } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { cn } from "@/lib/cn";
import { useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useUpdateWorkspaceHumanMemberRole, useWorkspaceMemberLists } from "./hooks";
import { useTransferWorkspaceOwnership } from "./member-mutations";
import { t } from "@/lib/i18n";

/** Stable empty reference so the avatar batch hook doesn't re-fetch on
 *  every render while members are still loading. */
const EMPTY_HUMANS: WorkspaceHumanMemberDto[] = [];

/** Roles a human can be reassigned to. `owner` goes through the separate
 *  transfer endpoint (PATCH role rejects owner-involving changes). */
const ASSIGNABLE_ROLES: readonly Exclude<WorkspaceRole, "owner">[] = ["admin", "member"];
const TRANSFER_OWNER = "transfer-owner";

const roleLabel = (): Record<WorkspaceRole, string> => ({
  owner: t("workspaces.owner"),
  admin: t("workspaces.admin"),
  member: t("workspaces.member"),
});

function viewerRoleLabel(role: WorkspaceRole | null | undefined): string {
  return role ? roleLabel()[role] : t("workspaces.nonMember");
}

/** Turn a PATCH failure into an operator-facing message. The backend gates
 *  on owner (403) and rejects owner-involving changes (400); both must be
 *  visible even though the front-end also gates. */
function describeRoleSaveError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.kind === "backend_unreachable") return t("workspaces.updateFailedCannotReach");
    if (err.status === 403) return t("workspaces.youDontHavePermission");
    if (err.status === 404) return t("workspaces.updateFailedMemberNo");
    if (err.status === 400) return t("workspaces.updateFailedThatRole");
    return t("workspaces.updateFailedStatus", { status: err.status });
  }
  return t("workspaces.updateFailed");
}

export interface HumanRolesPanelProps {
  workspaceId: string | null;
}

/**
 * Human-axis counterpart to `AgentPermissionsPanel`. Agents carry a
 * capability set; humans carry a single workspace `role`. Both live under
 * the dialog's Permission section as sibling tabs.
 *
 * Editing is owner-only and mirrors the backend gate exactly:
 *   - only owner viewers see an editable control (`viewerRole === 'owner'`);
 *   - the owner row itself is never editable, and `owner` is never an option;
 *   - so the only legal move is admin↔member, matching the route.
 * Non-owner viewers get read-only badges plus an explainer.
 */
export function HumanRolesPanel({ workspaceId }: HumanRolesPanelProps): JSX.Element {
  const membersQuery = useWorkspaceMemberLists(workspaceId);
  const humans = membersQuery.data?.humans ?? EMPTY_HUMANS;
  const viewerRole = membersQuery.data?.viewerRole;
  const canEdit = viewerRole === "owner";
  const avatarUrls = useUserAvatarObjectUrls(humans);
  const update = useUpdateWorkspaceHumanMemberRole(workspaceId);
  const transfer = useTransferWorkspaceOwnership(workspaceId);
  const [transferTarget, setTransferTarget] = useState<WorkspaceHumanMemberDto | null>(null);

  if (membersQuery.isLoading) {
    return <PanelSkeleton />;
  }
  if (membersQuery.isError) {
    return <PanelError onRetry={() => void membersQuery.refetch()} />;
  }
  if (humans.length === 0) {
    return (
      <PanelEmpty icon={<EmptyIcon className="size-6" />}>
        {t("workspaces.noPeopleInThis")}
      </PanelEmpty>
    );
  }

  const onChangeRole = (member: WorkspaceHumanMemberDto, role: Exclude<WorkspaceRole, "owner">) => {
    if (role === member.role) return;
    update.mutate(
      { userId: member.user_id, role },
      {
        onSuccess: () =>
          toast.success(
            t("workspaces.roleChanged", {
              name: member.display_name || member.name,
              role: roleLabel()[role],
            }),
          ),
        onError: (err) => toast.error(describeRoleSaveError(err)),
      },
    );
  };

  return (
    <div className="flex flex-col gap-4">
      {!canEdit ? (
        <div className="flex items-start gap-2 rounded-(--control-radius) border-(length:--control-border-width) border-border-subtle bg-bg-elevated/40 p-2.5 text-xs text-text-muted">
          <ErrorIcon className="mt-0.5 size-3.5 shrink-0" />
          <span>{t("workspaces.rolesReadOnly", { role: viewerRoleLabel(viewerRole) })}</span>
        </div>
      ) : null}

      <ul className="flex flex-col gap-1.5">
        {humans.map((member) => {
          const isOwner = member.role === "owner";
          // The owner row is never editable (backend rejects any
          // owner-involving change); only owner viewers may edit the rest.
          const editable = canEdit && !isOwner;
          return (
            <li
              key={member.user_id}
              className="flex items-center gap-2.5 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-floating px-3 py-2"
            >
              <UserAvatar
                size={AVATAR_PX.sm}
                avatarObjectUrl={avatarUrls.get(member.id) ?? null}
                alt={member.display_name || member.name}
              />
              <span className="flex min-w-0 flex-1 items-baseline gap-1.5">
                <span className="truncate text-sm font-medium text-text-primary">
                  {member.display_name || member.name}
                </span>
                <span className="truncate font-mono text-(length:--text-meta) text-text-muted">
                  @{member.name}
                </span>
              </span>
              {editable ? (
                <RoleSelect
                  value={member.role as Exclude<WorkspaceRole, "owner">}
                  disabled={update.isPending || transfer.isPending}
                  onChange={(role) => onChangeRole(member, role)}
                  onTransfer={() => setTransferTarget(member)}
                />
              ) : (
                <RoleBadge role={member.role} />
              )}
            </li>
          );
        })}
      </ul>
      <TransferOwnershipDialog
        target={transferTarget}
        pending={transfer.isPending}
        onCancel={() => setTransferTarget(null)}
        onConfirm={(member) =>
          transfer.mutate(member.user_id, {
            onSuccess: () => {
              setTransferTarget(null);
              toast.success(t("workspaces.nowOwner", { name: member.display_name || member.name }));
            },
            onError: (err) => toast.error(describeRoleSaveError(err)),
          })
        }
      />
    </div>
  );
}

function TransferOwnershipDialog({
  target,
  pending,
  onCancel,
  onConfirm,
}: {
  target: WorkspaceHumanMemberDto | null;
  pending: boolean;
  onCancel: () => void;
  onConfirm: (member: WorkspaceHumanMemberDto) => void;
}): JSX.Element {
  const name = target ? target.display_name || target.name : "";
  return (
    <Dialog
      open={target !== null}
      onOpenChange={(next) => (!next && !pending ? onCancel() : undefined)}
    >
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>{t("workspaces.transferTitle", { name })}</DialogTitle>
        </DialogHeader>
        <DialogDescription className="text-sm leading-relaxed text-text-muted">
          {t("workspaces.transferDetail", { name })}
        </DialogDescription>
        <DialogFooter>
          <Button variant="cancel" onClick={onCancel} disabled={pending}>
            {t("common.cancel")}
          </Button>
          <Button variant="primary" onClick={() => target && onConfirm(target)} disabled={pending}>
            {pending ? t("workspaces.transferring") : t("workspaces.transferOwnership")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Read-only role pill. Owner uses a neutral-but-distinct emphasis; admin /
 *  member stay plain so the strip doesn't read like a status signal. */
function RoleBadge({ role }: { role: WorkspaceRole }): JSX.Element {
  return (
    <span
      className={cn(
        "shrink-0 rounded-(--chip-radius) border-(length:--control-border-width) px-2 py-0.5 text-(length:--text-meta) font-medium",
        role === "owner"
          ? "border-border text-text-secondary"
          : "border-border-subtle text-text-muted",
      )}
    >
      {roleLabel()[role]}
    </span>
  );
}

function RoleSelect({
  value,
  disabled,
  onChange,
  onTransfer,
}: {
  value: Exclude<WorkspaceRole, "owner">;
  disabled: boolean;
  onChange: (role: Exclude<WorkspaceRole, "owner">) => void;
  onTransfer: () => void;
}): JSX.Element {
  return (
    <Select.Root
      value={value}
      onValueChange={(v) =>
        v === TRANSFER_OWNER ? onTransfer() : onChange(v as Exclude<WorkspaceRole, "owner">)
      }
      disabled={disabled}
    >
      <Select.Trigger
        aria-label={t("workspaces.memberRole")}
        className={cn(
          "flex h-7 shrink-0 items-center justify-between gap-1.5 rounded-(--control-radius) bg-bg-base px-2.5 text-xs",
          "border-(length:--control-border-width) border-border transition-colors",
          "hover:border-(--border-control-hover)",
          "data-[state=open]:border-border-focus focus-visible:outline-none focus-visible:border-border-focus",
          "disabled:cursor-not-allowed disabled:opacity-60",
        )}
      >
        <Select.Value />
        <Select.Icon>
          <ChevronDownIcon className="size-3 shrink-0 text-text-muted" />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content
          position="popper"
          align="end"
          sideOffset={4}
          className="z-(--z-in-dialog) min-w-40 rounded-(--control-radius) border-(length:--control-border-width) border-border bg-bg-floating p-1 text-sm shadow-popover"
        >
          <Select.Viewport>
            {ASSIGNABLE_ROLES.map((role) => (
              <Select.Item
                key={role}
                value={role}
                className="flex cursor-default items-center gap-2 rounded-(--control-radius) px-2 py-1.5 text-text-primary outline-none data-highlighted:bg-bg-hover"
              >
                <span className="flex w-3.5 shrink-0 justify-center">
                  <Select.ItemIndicator>
                    <CheckIcon className="size-3" />
                  </Select.ItemIndicator>
                </span>
                <Select.ItemText asChild>
                  <span className="text-xs font-medium">{roleLabel()[role]}</span>
                </Select.ItemText>
              </Select.Item>
            ))}
            <Select.Separator className="my-1 h-px bg-border-subtle" />
            <Select.Item
              value={TRANSFER_OWNER}
              className="flex cursor-default items-center gap-2 rounded-(--control-radius) px-2 py-1.5 text-text-primary outline-none data-highlighted:bg-bg-hover"
            >
              <span className="w-3.5 shrink-0" />
              <Select.ItemText asChild>
                <span className="text-xs font-medium">{t("workspaces.transferOwnership")}</span>
              </Select.ItemText>
            </Select.Item>
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

function PanelSkeleton(): JSX.Element {
  return (
    <div className="flex flex-col gap-1.5">
      {[0, 1, 2].map((i) => (
        <div
          key={i}
          className="h-11 animate-pulse rounded-(--control-radius) border border-border-subtle bg-bg-elevated/40"
        />
      ))}
    </div>
  );
}
