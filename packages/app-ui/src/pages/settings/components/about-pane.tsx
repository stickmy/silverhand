import { useEffect, useState } from "react";
import { OpenExternalIcon } from "@/icons";
import { getPlatform } from "@/platform";
import { ExternalLink, PRIVACY_POLICY_URL, SUPPORT_URL } from "@/features/legal/legal-links";
import { AboutVersionRow } from "./about-version-row";
import { SettingsGroup, SettingsPage, SettingsSection } from "./settings-section";
import { t } from "@/lib/i18n";

const legalLinks = () =>
  [
    { label: t("appShell.privacyPolicy"), href: PRIVACY_POLICY_URL },
    { label: t("settings.about.support"), href: SUPPORT_URL },
  ] as const;

export function AboutPane(): JSX.Element {
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void getPlatform()
      .getAppVersion?.()
      .then((value) => {
        if (!cancelled && value) setVersion(value);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <SettingsPage>
      <SettingsSection label={t("settings.about.app")}>
        <SettingsGroup>
          {version ? <AboutVersionRow version={version} /> : null}
          {legalLinks().map(({ label, href }) => (
            <ExternalLink
              key={href}
              href={href}
              data-settings-row
              className="flex min-h-12 items-center justify-between gap-3 px-4 py-2.5 text-sm font-semibold text-text-primary transition-colors hover:bg-bg-hover"
            >
              {label}
              <OpenExternalIcon aria-hidden className="size-3.5 shrink-0 text-text-muted" />
            </ExternalLink>
          ))}
        </SettingsGroup>
      </SettingsSection>
    </SettingsPage>
  );
}
