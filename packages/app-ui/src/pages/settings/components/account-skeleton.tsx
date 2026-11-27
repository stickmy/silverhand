import { SkeletonBlock as Block, SkeletonRegion as Region } from "@/components/layout/skeleton";
import { AVATAR_PX } from "@/components/avatar-scale";
import { t } from "@/lib/i18n";
import { SettingsGroup, SettingsPage, SettingsRow, SettingsSection } from "./settings-section";

export function AccountSkeleton(): JSX.Element {
  return (
    <SettingsPage>
      <Region label={t("settings.account.loading")} className="flex flex-col gap-6">
        <SettingsSection label={t("settings.account.profile")}>
          <SettingsGroup className="gap-4 p-4">
            <div className="flex items-center gap-3.5">
              <div style={{ width: AVATAR_PX["3xl"], height: AVATAR_PX["3xl"] }}>
                <Block className="size-full" />
              </div>
              <div className="space-y-2">
                <Block className="h-4 w-32" />
                <Block className="h-3 w-20" />
              </div>
            </div>
            <div className="space-y-1.5">
              <p className="text-sm text-text-muted">{t("appShell.displayName")}</p>
              <Block className="h-8 w-full" />
            </div>
          </SettingsGroup>
        </SettingsSection>
        <SettingsSection label={t("settings.account.session")}>
          <SettingsGroup>
            <SettingsRow
              label={<Block className="h-3.5 w-24" />}
              control={<Block className="h-7 w-20" />}
            />
          </SettingsGroup>
        </SettingsSection>
      </Region>
    </SettingsPage>
  );
}
