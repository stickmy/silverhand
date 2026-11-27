import { useEffect, useId, useState, type JSX } from "react";
import { useQuery } from "@tanstack/react-query";
import { apiPaths } from "@silverhand/shared";
import { api, ApiError } from "@/lib/api";
import { useLogout } from "@/features/session/hooks";
import { useServerFeature } from "@/features/session/server-environment";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { t } from "@/lib/i18n";
import { SettingsRow } from "./settings-section";

interface Blocker {
  id: string;
  name: string;
}
interface Blockers {
  agents: Blocker[];
  workspaces: Blocker[];
}
interface Preflight {
  blockers: Blockers | null;
}

function BlockerGroup({ label, items }: { label: string; items: Blocker[] }): JSX.Element | null {
  if (items.length === 0) return null;
  return (
    <div className="flex flex-col gap-1">
      <p className="font-medium text-text-secondary">{label}</p>
      <ul className="list-disc pl-4">
        {items.map((item) => (
          <li key={item.id}>{item.name}</li>
        ))}
      </ul>
    </div>
  );
}

function BlockerList({ blockers }: { blockers: Blockers }): JSX.Element {
  return (
    <div className="flex flex-col gap-3 text-xs text-text-muted">
      <p>{t("settings.delete.resolveFirst")}</p>
      <BlockerGroup label={t("settings.delete.projects")} items={blockers.workspaces} />
      <BlockerGroup
        label={t("settings.delete.agents")}
        items={blockers.agents.map((a) => ({ id: a.id, name: `@${a.name}` }))}
      />
    </div>
  );
}

/** Settings › Account danger zone, shown where the server offers account deletion. */
export function DeleteAccountSection({ handle }: { handle: string }): JSX.Element | null {
  const offered = useServerFeature("account_deletion_enabled");
  const [open, setOpen] = useState(false);
  const preflight = useQuery({
    queryKey: ["account-deletion"],
    queryFn: () => api<Preflight>(apiPaths.meDeletion),
    enabled: offered,
  });

  if (!offered) {
    return null;
  }

  return (
    <>
      <SettingsRow
        label={t("settings.delete.title")}
        description={t("settings.delete.permanent")}
        control={
          <Button
            type="button"
            variant="destructive-outline"
            size="sm"
            onClick={() => {
              void preflight.refetch();
              setOpen(true);
            }}
          >
            {t("settings.delete.title")}
          </Button>
        }
      />
      <DeleteAccountDialog
        open={open}
        onOpenChange={setOpen}
        handle={handle}
        blockers={preflight.data?.blockers ?? null}
      />
    </>
  );
}

function DeleteAccountDialog({
  open,
  onOpenChange,
  handle,
  blockers,
}: {
  open: boolean;
  onOpenChange: (next: boolean) => void;
  handle: string;
  blockers: Blockers | null;
}): JSX.Element {
  const id = useId();
  const logout = useLogout();
  const [password, setPassword] = useState("");
  const [typed, setTyped] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState<Blockers | null>(null);

  useEffect(() => {
    if (!open) return;
    setPassword("");
    setTyped("");
    setError(null);
    setConflict(null);
  }, [open]);

  const shown = conflict ?? blockers;
  const blocked = !!shown && (shown.agents.length > 0 || shown.workspaces.length > 0);

  const submit = async () => {
    setPending(true);
    setError(null);
    try {
      await api(apiPaths.me, {
        method: "DELETE",
        body: JSON.stringify({ password, handle: typed }),
      });
      // Sessions are already revoked server-side; logout clears the local session.
      await logout.mutateAsync();
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        try {
          setConflict((JSON.parse(err.body) as { blockers: Blockers }).blockers);
        } catch {
          setError(t("settings.delete.ownsThings"));
        }
      } else if (err instanceof ApiError && err.status === 403) {
        setError(t("settings.delete.incorrect"));
      } else {
        setError(t("settings.delete.failed"));
      }
      setPending(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(next) => (pending ? undefined : onOpenChange(next))}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>{t("settings.delete.confirmTitle")}</DialogTitle>
        </DialogHeader>
        <DialogDescription className="text-sm leading-relaxed text-text-muted">
          {t("settings.delete.confirmBody")}
        </DialogDescription>
        {blocked && shown ? (
          <BlockerList blockers={shown} />
        ) : (
          <form
            id={`${id}-form`}
            className="flex flex-col gap-3"
            onSubmit={(event) => {
              event.preventDefault();
              void submit();
            }}
          >
            <Label htmlFor={`${id}-password`}>{t("appShell.password")}</Label>
            <Input
              id={`${id}-password`}
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              disabled={pending}
              required
              maxLength={1024}
            />
            <Label htmlFor={`${id}-handle`}>{t("settings.delete.typeHandle", { handle })}</Label>
            <Input
              id={`${id}-handle`}
              autoComplete="off"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              disabled={pending}
              required
            />
          </form>
        )}
        {error ? (
          <p className="text-xs text-error" role="alert">
            {error}
          </p>
        ) : null}
        <DialogFooter>
          <Button variant="cancel" onClick={() => onOpenChange(false)} disabled={pending}>
            {t("common.cancel")}
          </Button>
          {blocked ? null : (
            <Button
              variant="destructive"
              type="submit"
              form={`${id}-form`}
              disabled={pending || !password || typed !== handle}
            >
              {pending ? t("settings.delete.deleting") : t("settings.delete.title")}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
