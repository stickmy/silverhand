import { useEffect, useState } from "react";
import { InfoIcon, OpenExternalIcon } from "@/icons";
import { getPlatform } from "@/platform";
import { ExternalLink, PRIVACY_POLICY_URL, SUPPORT_URL } from "@/features/legal/legal-links";
import { AboutVersionRow } from "./about-version-row";
import { SettingsHeading } from "./settings-heading";
import { t } from "@/lib/i18n";

const legalLinks = () =>
  [
    { label: t("appShell.privacyPolicy"), href: PRIVACY_POLICY_URL },
    { label: t("settings.about.support"), href: SUPPORT_URL },
  ] as const;

const ROW = "flex min-h-11 items-center justify-between gap-3 px-3 text-sm";

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
    <section className="flex min-w-0 flex-1 flex-col gap-5 px-5 py-5">
      <SettingsHeading
        icon={<InfoIcon className="size-4" />}
        title={t("settings.sections.about")}
      />

      <div className="flex max-w-2xl flex-col divide-y divide-border-subtle rounded-(--control-radius) border border-border-subtle">
        {version ? <AboutVersionRow version={version} /> : null}
        {legalLinks().map(({ label, href }) => (
          <ExternalLink
            key={href}
            href={href}
            className={`${ROW} text-text-primary hover:bg-bg-hover`}
          >
            {label}
            <OpenExternalIcon aria-hidden className="size-3.5 shrink-0 text-text-muted" />
          </ExternalLink>
        ))}
      </div>
    </section>
  );
}
