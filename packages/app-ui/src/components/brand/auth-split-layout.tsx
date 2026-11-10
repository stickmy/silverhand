import type { ReactNode } from "react";
import { Brandmark } from "./brandmark";
import { t } from "@/lib/i18n";

/** Split shell shared by the auth gate and the profile-setup step: a narrow
 *  brand column on the left, the form on the open page to the right. Below
 *  `min-[860px]` the brand column is hidden and a compact brand header sits
 *  above the form. */
export function AuthSplitLayout({ children }: { children: ReactNode }): JSX.Element {
  return (
    <main
      className="auth-layout flex h-screen w-screen overflow-y-auto bg-bg-base text-text-primary"
      style={{ WebkitAppRegion: "drag" } as React.CSSProperties}
    >
      <BrandPanel />
      <div className="auth-content grid w-full place-items-center px-6 py-14 min-[860px]:flex-1">
        <header className="auth-mobile-brand mb-8 hidden">
          <div className="flex items-center gap-2">
            <Brandmark className="size-6 shrink-0 text-text-primary" />
            <span className="text-base font-semibold tracking-tight">Silverhand</span>
          </div>
        </header>
        {children}
      </div>
    </main>
  );
}

const ISO_GRID =
  "repeating-linear-gradient(30deg, var(--border-subtle) 0 1px, transparent 1px 88px)," +
  "repeating-linear-gradient(150deg, var(--border-subtle) 0 1px, transparent 1px 88px)";

const GRID_FADE = "linear-gradient(to top, #000 10%, transparent 75%)";

function BrandPanel(): JSX.Element {
  return (
    <section
      aria-hidden
      data-auth-brand
      className="relative hidden w-[44%] shrink-0 overflow-hidden border-r border-border bg-(--auth-brand-bg,color-mix(in_oklab,var(--text-primary)_3%,var(--bg-base))) min-[860px]:block"
    >
      <div
        className="absolute inset-0"
        style={{ backgroundImage: ISO_GRID, maskImage: GRID_FADE, WebkitMaskImage: GRID_FADE }}
      />
      <div className="relative flex h-full flex-col justify-between px-14 pt-18 pb-12">
        <Brandmark className="size-11 text-text-primary" />
        <div className="flex flex-col gap-5">
          <h2 className="text-6xl font-semibold leading-none tracking-tighter">Silverhand</h2>
          <p className="max-w-96 text-base leading-relaxed text-text-muted">
            {t("ui.whereHumansAndAgents")}
          </p>
        </div>
        <p className="text-xs tracking-wide text-text-muted">silverhand</p>
      </div>
    </section>
  );
}
