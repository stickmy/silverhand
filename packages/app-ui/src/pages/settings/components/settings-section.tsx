import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import "./settings-section.css";

/** Scrollable single-column body shared by every Settings detail pane. */
export function SettingsPage({
  children,
  className,
  ...rest
}: {
  children: ReactNode;
  className?: string;
} & React.HTMLAttributes<HTMLElement>): JSX.Element {
  return (
    <section
      className={cn("min-h-0 min-w-0 flex-1 overflow-y-auto px-4 py-5 sm:px-6 sm:py-6", className)}
      {...rest}
    >
      <div className="mx-auto flex w-full min-w-0 max-w-2xl flex-col gap-6">{children}</div>
    </section>
  );
}

/** Labelled settings section: an outside section label above a grouped surface,
 *  with an optional footer for status banners or validation copy. */
export function SettingsSection({
  id,
  label,
  footer,
  children,
}: {
  id?: string;
  label?: string;
  footer?: ReactNode;
  children: ReactNode;
}): JSX.Element {
  return (
    <section id={id} className="flex min-w-0 scroll-mt-6 flex-col">
      {label ? (
        <h3
          data-settings-section-label
          className="px-1 pb-2 text-[13px] font-semibold text-text-secondary"
        >
          {label}
        </h3>
      ) : null}
      <div className="min-w-0">{children}</div>
      {footer ? <div className="px-1 pt-2">{footer}</div> : null}
    </section>
  );
}

/** Inset grouped card that holds one or more `SettingsRow` entries. */
export function SettingsGroup({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}): JSX.Element {
  return (
    <div
      data-settings-group
      className={cn(
        "flex min-w-0 flex-col overflow-hidden rounded-(--card-radius) border border-border-subtle bg-[color-mix(in_srgb,var(--text-primary)_2.8%,var(--bg-base))]",
        className,
      )}
    >
      {children}
    </div>
  );
}

/** Standard row inside a `SettingsGroup`: left label + optional description,
 *  right control. Pass `stack` for multi-band rows (e.g. font presets + preview)
 *  or `wrapMobile` when the right control needs full width on narrow viewports. */
export function SettingsRow({
  label,
  description,
  htmlFor,
  control,
  stack = false,
  wrapMobile = false,
  className,
  children,
}: {
  label?: ReactNode;
  description?: ReactNode;
  htmlFor?: string;
  control?: ReactNode;
  stack?: boolean;
  wrapMobile?: boolean;
  className?: string;
  children?: ReactNode;
}): JSX.Element {
  if (stack) {
    return (
      <div data-settings-row className={cn("flex min-w-0 flex-col gap-2.5 px-4 py-3", className)}>
        {children}
      </div>
    );
  }

  return (
    <div
      data-settings-row
      className={cn(
        "flex min-h-12 min-w-0 gap-3 px-4 py-2.5",
        wrapMobile
          ? "flex-col items-stretch sm:flex-row sm:items-center sm:justify-between sm:gap-4"
          : "items-center justify-between gap-4",
        className,
      )}
    >
      {children ?? (
        <>
          <div className="min-w-0 flex-1">
            {label ? (
              htmlFor ? (
                <label
                  htmlFor={htmlFor}
                  className="block text-sm font-semibold leading-snug text-text-primary"
                >
                  {label}
                </label>
              ) : (
                <div className="text-sm font-semibold leading-snug text-text-primary">{label}</div>
              )
            ) : null}
            {description ? (
              <p className="mt-0.5 text-xs leading-snug text-text-muted">{description}</p>
            ) : null}
          </div>
          {control ? (
            <div
              className={cn(
                "flex shrink-0 items-center gap-2",
                wrapMobile && "w-full sm:w-auto sm:justify-end",
              )}
            >
              {control}
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}
