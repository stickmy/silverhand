import { useCompactLayout } from "@/navigation/layout";
import { useLocation } from "react-router-dom";
import { useNav } from "@/router";
import { AccountEmail } from "./components/account-email";
import { AccountSkeleton } from "./components/account-skeleton";
import { ChangePasswordForm } from "./components/change-password-form";
import { DeleteAccountSection } from "./components/delete-account-section";
import { LanguagePane } from "./components/language-pane";
import { RangeInput } from "@/components/ui/range-input";
import { useEffect, useState, type CSSProperties, type ReactNode } from "react";
import {
  AppearanceIcon,
  InfoIcon,
  LanguageIcon,
  LeaveIcon,
  ServerIcon,
  SettingsIcon,
  SpinnerIcon,
  UndoIcon,
  UserIcon,
} from "@/icons";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { SidebarRow, SidebarSection } from "@/components/layout/sidebar-shared";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";
import { useAppearance } from "@/features/appearance/use-appearance";
import {
  SIDEBAR_STYLES,
  setSidebarStyle,
  useSidebarStyle,
  type SidebarStyle,
} from "@/features/conversation-shell/use-sidebar-style";
import {
  MESSAGE_LAYOUT_MODES,
  setMessageLayoutMode,
  useMessageLayoutMode,
  type MessageLayoutMode,
} from "@/features/messages/hooks/use-message-layout-mode";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useLogout,
  useViewerUser,
} from "@/features/session/hooks";
import { ViewerProfileEditor } from "@/pages/settings/components/viewer-profile-editor";
import {
  SettingsGroup,
  SettingsPage,
  SettingsRow,
  SettingsSection,
} from "./components/settings-section";
import {
  DEFAULT_MARKDOWN_FONT_SIZE,
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  THEME_IDS,
  type ThemeId,
} from "@/features/appearance/types";
import {
  codeFontPresets,
  proseFontPresets,
  type FontPreset,
} from "@/features/appearance/font-presets";
import {
  SETTINGS_SECTIONS,
  persistSettingsSection,
  readSettingsSection,
  type SettingsSection as SettingsSectionId,
} from "../../features/settings/model/settings-section";
import { ConnectionPane } from "./components/connection-pane";
import { AboutPane } from "./components/about-pane";
import { PaneHeader, PaneShell } from "@/components/layout/pane-shell";
import { Sidebar } from "@/components/layout/sidebar";
import { SidebarSlot } from "@/components/layout/sidebar-slot";

/** 4-color swatch preview and display name for each theme. */
const THEME_META: Record<
  ThemeId,
  {
    label: string;
    bg: string;
    surface: string;
    text: string;
    accent: string;
  }
> = {
  "studio-light": {
    label: "Studio Light",
    bg: "#F2F5F9",
    surface: "#DEE3EB",
    text: "#111725",
    accent: "#2F5AD0",
  },
  poster: {
    label: "Poster",
    bg: "#FFFDF7",
    surface: "#F2EFE8",
    text: "#0D0C0B",
    accent: "#2448D8",
  },
  "slack-dark": {
    label: "Slack Dark",
    bg: "#19171D",
    surface: "#350D36",
    text: "#E2E3E3",
    accent: "#1D9BD1",
  },
  iris: {
    label: "Iris",
    bg: "#E6E6B6",
    surface: "#C4D0DA",
    text: "#1F1A12",
    accent: "#6D28C9",
  },
  glass: {
    label: "Glass",
    bg: "#EEF3F6",
    surface: "#FFFFFF",
    text: "#16191D",
    accent: "#0071E3",
  },
};

export function SettingsShell(): JSX.Element {
  const {
    appearance,
    setTheme,
    setMarkdownFont,
    setMarkdownFontSize,
    resetMarkdownText,
    resetMarkdownFontSize,
  } = useAppearance();

  const compact = useCompactLayout();
  const location = useLocation();
  const nav = useNav();
  const sectionParam = new URLSearchParams(location.search).get("settingsSection");
  const [rememberedSection, setRememberedSection] =
    useState<SettingsSectionId>(readSettingsSection);
  const section = SETTINGS_SECTIONS.includes(sectionParam as SettingsSectionId)
    ? (sectionParam as SettingsSectionId)
    : rememberedSection;
  const setSection = (next: SettingsSectionId) => {
    setRememberedSection(next);
    nav.go(`/settings?settingsSection=${next}`);
  };
  useEffect(() => {
    persistSettingsSection(section);
  }, [section]);

  const content = (
    <PaneShell>
      <PaneHeader icon={sectionChrome()[section].icon} title={sectionChrome()[section].label} />

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        {section === "appearance" ? (
          <AppearancePane
            appearance={appearance}
            setTheme={setTheme}
            setMarkdownFont={setMarkdownFont}
            setMarkdownFontSize={setMarkdownFontSize}
            resetMarkdownText={resetMarkdownText}
            resetMarkdownFontSize={resetMarkdownFontSize}
          />
        ) : section === "account" ? (
          <AccountPane />
        ) : section === "language" ? (
          <LanguagePane />
        ) : section === "about" ? (
          <AboutPane />
        ) : (
          <ConnectionPane />
        )}
      </div>
    </PaneShell>
  );
  if (compact)
    return sectionParam ? (
      content
    ) : (
      <Sidebar
        name="settings"
        fill
        sidebar={<SettingsRailNav section={null} bare onSelect={setSection} />}
      />
    );
  return (
    <>
      <SidebarSlot>
        <Sidebar
          name="settings"
          label={t("settings.sectionsLabel")}
          width="var(--sidebar-width)"
          sidebar={<SettingsRailNav section={section} onSelect={setSection} />}
        />
      </SidebarSlot>
      {content}
    </>
  );
}

/** The Settings sidebar as the rail's hover peek. */
export function SettingsPeekSidebar(): JSX.Element {
  const location = useLocation();
  const nav = useNav();
  const sectionParam = new URLSearchParams(location.search).get("settingsSection");
  const current =
    location.pathname === "/settings"
      ? SETTINGS_SECTIONS.includes(sectionParam as SettingsSectionId)
        ? (sectionParam as SettingsSectionId)
        : readSettingsSection()
      : null;

  return (
    <Sidebar
      name="settings"
      label={t("settings.sectionsLabel")}
      width="var(--sidebar-width)"
      sidebar={
        <SettingsRailNav
          section={current}
          onSelect={(next) => {
            persistSettingsSection(next);
            nav.go(`/settings?settingsSection=${next}`);
          }}
        />
      }
    />
  );
}

const sectionChrome = (): Record<SettingsSectionId, { label: string; icon: ReactNode }> => ({
  account: { label: t("settings.sections.account"), icon: <UserIcon className="size-3.5" /> },
  language: {
    label: t("settings.sections.language"),
    icon: <LanguageIcon className="size-3.5" />,
  },
  appearance: {
    label: t("settings.sections.appearance"),
    icon: <AppearanceIcon className="size-3.5" />,
  },
  connection: {
    label: t("settings.sections.connection"),
    icon: <ServerIcon className="size-3.5" />,
  },
  about: { label: t("settings.sections.about"), icon: <InfoIcon className="size-3.5" /> },
});

function SettingsRailNav({
  section,
  bare = false,
  onSelect,
}: {
  /** Null on the compact list page: no detail pane is showing, so no row is current. */
  section: SettingsSectionId | null;
  /** Drop the group header when the page title already names the only group. */
  bare?: boolean;
  onSelect: (next: SettingsSectionId) => void;
}): JSX.Element {
  const rows = SETTINGS_SECTIONS.map((id) => {
    const { label, icon } = sectionChrome()[id];
    const selected = id === section;

    return (
      <SidebarRow
        key={id}
        icon={icon}
        title={label}
        selected={selected}
        aria-current={selected ? "page" : undefined}
        onClick={() => onSelect(id)}
      />
    );
  });

  return (
    <nav className="scrollbar-hide flex-1 overflow-y-auto py-1.5">
      {bare ? (
        <ul className="space-y-0.5">{rows}</ul>
      ) : (
        <SidebarSection
          label={t("settings.title")}
          icon={<SettingsIcon className="size-3.5" />}
          collapsible
          defaultExpanded
          persistId="settings-directory"
        >
          {rows}
        </SidebarSection>
      )}
    </nav>
  );
}

/** Appearance pane — theme picker + layout + Markdown Text controls. Settings
 *  here auto-persist to localStorage via {@link useAppearance}; no Save
 *  button needed, so this pane has no footer. */
function AppearancePane({
  appearance,
  setTheme,
  setMarkdownFont,
  setMarkdownFontSize,
  resetMarkdownText,
  resetMarkdownFontSize,
}: {
  appearance: ReturnType<typeof useAppearance>["appearance"];
  setTheme: ReturnType<typeof useAppearance>["setTheme"];
  setMarkdownFont: ReturnType<typeof useAppearance>["setMarkdownFont"];
  setMarkdownFontSize: ReturnType<typeof useAppearance>["setMarkdownFontSize"];
  resetMarkdownText: ReturnType<typeof useAppearance>["resetMarkdownText"];
  resetMarkdownFontSize: ReturnType<typeof useAppearance>["resetMarkdownFontSize"];
}): JSX.Element {
  return (
    <SettingsPage data-appearance-pane>
      <SettingsSection id="settings-appearance" label={t("settings.appearance.theme")}>
        <SettingsGroup>
          <div
            role="radiogroup"
            aria-label={t("settings.appearance.theme")}
            className="grid grid-cols-3 gap-2.5 p-3.5 sm:grid-cols-5"
          >
            {THEME_IDS.map((id) => {
              const meta = THEME_META[id];
              const active = appearance.theme === id;
              return (
                <button
                  key={id}
                  type="button"
                  role="radio"
                  aria-checked={active}
                  data-settings-theme-tile
                  onClick={() => setTheme(id)}
                  className="group flex min-w-0 flex-col items-center gap-1.5 rounded-(--control-radius) p-0.5 text-center focus-outline"
                >
                  <span
                    data-settings-theme-preview
                    className={cn(
                      "flex aspect-[16/11] w-full flex-col justify-between overflow-hidden rounded-[calc(var(--control-radius)+1px)] border-[1.5px] p-1.5 transition-[translate,transform,border-color,box-shadow] duration-200 ease-out group-hover:-translate-y-0.5",
                      active ? "border-accent ring-2 ring-(--accent-soft)" : "border-border",
                    )}
                    style={{
                      background: `linear-gradient(135deg, ${meta.bg}, ${meta.surface})`,
                    }}
                  >
                    <span className="flex gap-1">
                      {[meta.bg, meta.text, meta.accent].map((c) => (
                        <span
                          key={c}
                          className="size-2 rounded-full border border-black/12"
                          style={{ background: c }}
                        />
                      ))}
                    </span>
                    <span className="flex flex-col gap-1">
                      <span
                        className="h-1 w-3/4 rounded-full opacity-25"
                        style={{ background: meta.text }}
                      />
                      <span
                        className="h-1 w-1/2 rounded-full opacity-50"
                        style={{ background: meta.accent }}
                      />
                    </span>
                  </span>
                  <span
                    className={cn(
                      "max-w-full truncate text-xs",
                      active
                        ? "font-semibold text-text-primary"
                        : "font-medium text-text-secondary",
                    )}
                  >
                    {meta.label}
                  </span>
                </button>
              );
            })}
          </div>
        </SettingsGroup>
      </SettingsSection>

      <SettingsSection label={t("settings.appearance.layout")}>
        <SettingsGroup>
          <MessageAlignmentRow />
          <SidebarStyleRow />
        </SettingsGroup>
      </SettingsSection>

      <SettingsSection id="settings-markdown-text" label={t("settings.appearance.markdownText")}>
        <SettingsGroup>
          <SharedFontSizeRow
            value={appearance.fontSize}
            onChange={(value) => setMarkdownFontSize(value)}
            onReset={() => resetMarkdownFontSize()}
          />
          <MarkdownFontRow
            title={t("settings.appearance.prose")}
            fontFamily={appearance.prose?.fontFamily}
            fontSize={appearance.fontSize}
            presets={proseFontPresets()}
            sample={t("settings.appearance.proseSample")}
            onFontChange={(value) => setMarkdownFont("prose", value || undefined)}
            onReset={() => resetMarkdownText("prose")}
          />
          <MarkdownFontRow
            title={t("settings.appearance.code")}
            fontFamily={appearance.code?.fontFamily}
            fontSize={appearance.fontSize}
            presets={codeFontPresets()}
            sample="const size = 12"
            onFontChange={(value) => setMarkdownFont("code", value || undefined)}
            onReset={() => resetMarkdownText("code")}
            code
          />
        </SettingsGroup>
      </SettingsSection>
    </SettingsPage>
  );
}

const sidebarStyleMeta = (): Record<SidebarStyle, string> => ({
  tree: t("settings.appearance.sidebar.tree"),
  recent: t("settings.appearance.sidebar.recent"),
});

function SidebarStyleRow(): JSX.Element {
  const style = useSidebarStyle();

  return (
    <SettingsRow
      label={t("settings.appearance.sidebar.title")}
      wrapMobile
      control={
        <div
          role="radiogroup"
          aria-label={t("settings.appearance.sidebar.title")}
          className="inline-flex gap-2"
        >
          {SIDEBAR_STYLES.map((id) => {
            const selected = id === style;

            return (
              <button
                key={id}
                type="button"
                role="radio"
                aria-checked={selected}
                data-settings-mini-choice
                onClick={() => setSidebarStyle(id)}
                className={cn(
                  "flex w-25 flex-col items-center gap-1.5 rounded-(--control-radius) border-(length:--control-border-width) px-2 py-1.5 text-[11px] font-medium transition-colors focus-outline",
                  selected
                    ? "border-accent bg-accent-soft text-accent"
                    : "border-border bg-bg-floating text-text-secondary hover:border-(--border-control-hover) hover:text-text-primary",
                )}
              >
                <div aria-hidden="true" className="flex h-6.5 w-full flex-col justify-center gap-1">
                  {id === "tree"
                    ? [0, 1].map((row) => (
                        <div
                          key={row}
                          className={cn("flex items-center gap-1", row === 1 && "pl-2")}
                        >
                          <span className="size-1.5 shrink-0 rounded-[2px] bg-current opacity-55" />
                          <span
                            className={cn(
                              "h-1 rounded-[2px] bg-current opacity-30",
                              row === 0 ? "w-1/2" : "w-2/3",
                            )}
                          />
                        </div>
                      ))
                    : [0, 1].map((row) => (
                        <div key={row} className="flex items-center gap-1">
                          <span className="size-2 shrink-0 rounded-[2px] bg-current opacity-55" />
                          <span className="h-1 flex-1 rounded-[2px] bg-current opacity-30" />
                        </div>
                      ))}
                </div>
                <span className="truncate">{sidebarStyleMeta()[id]}</span>
              </button>
            );
          })}
        </div>
      }
    />
  );
}

const messageLayoutMeta = (): Record<MessageLayoutMode, string> => ({
  "all-left": t("settings.appearance.alignment.allLeft"),
  "user-right": t("settings.appearance.alignment.mineRight"),
});

function MessageAlignmentRow(): JSX.Element {
  const mode = useMessageLayoutMode();

  return (
    <SettingsRow
      label={t("settings.appearance.alignment.title")}
      wrapMobile
      control={
        <div
          role="radiogroup"
          aria-label={t("settings.appearance.alignment.title")}
          className="inline-flex gap-2"
        >
          {MESSAGE_LAYOUT_MODES.map((id) => {
            const selected = id === mode;

            return (
              <button
                key={id}
                type="button"
                role="radio"
                aria-checked={selected}
                aria-label={messageLayoutMeta()[id]}
                data-settings-mini-choice
                onClick={() => setMessageLayoutMode(id)}
                className={cn(
                  "flex w-25 flex-col items-center gap-1.5 rounded-(--control-radius) border-(length:--control-border-width) px-2 py-1.5 text-[11px] font-medium transition-colors focus-outline",
                  selected
                    ? "border-accent bg-accent-soft text-accent"
                    : "border-border bg-bg-floating text-text-secondary hover:border-(--border-control-hover) hover:text-text-primary",
                )}
              >
                <div aria-hidden="true" className="flex h-6.5 w-full flex-col justify-center gap-1">
                  {[0, 1].map((row) => (
                    <div
                      key={row}
                      className={cn(
                        "flex w-4/5 items-center gap-1",
                        id === "user-right" && row === 1 && "self-end flex-row-reverse",
                      )}
                    >
                      <span className="size-1.5 shrink-0 rounded-[2px] bg-current opacity-55" />
                      <span className="h-1 flex-1 rounded-[2px] bg-current opacity-30" />
                    </div>
                  ))}
                </div>
                <span className="truncate">{messageLayoutMeta()[id]}</span>
              </button>
            );
          })}
        </div>
      }
    />
  );
}

function AccountPane(): JSX.Element {
  const viewer = useViewerUser();
  const logout = useLogout();
  const [error, setError] = useState<string | null>(null);

  const dto =
    viewer.data &&
    !isRegistrationRequired(viewer.data) &&
    !isBackendUnreachable(viewer.data) &&
    !isUpdateRequired(viewer.data)
      ? viewer.data
      : undefined;

  const onLogout = async () => {
    setError(null);
    try {
      await logout.mutateAsync();
    } catch (err) {
      setError(String(err));
    }
  };

  if (viewer.isLoading) {
    return <AccountSkeleton />;
  }
  if (!dto) {
    return (
      <section className="flex min-w-0 flex-1 items-center justify-center py-10 text-xs text-text-muted">
        {t("settings.account.loadFailed")}
      </section>
    );
  }

  return (
    <SettingsPage>
      <ViewerProfileEditor user={dto} email={<AccountEmail key={dto.id} userId={dto.id} />} />

      <ChangePasswordForm key={dto.id} userId={dto.id} />

      <SettingsSection
        label={t("settings.account.session")}
        footer={
          error ? (
            <p className="text-xs text-error" role="alert">
              {error}
            </p>
          ) : null
        }
      >
        <SettingsGroup>
          <SettingsRow
            label={t("settings.account.logOut")}
            description={t("settings.account.logOutDetail")}
            control={
              <Button
                type="button"
                variant="destructive-outline"
                size="sm"
                onClick={() => void onLogout()}
                disabled={logout.isPending}
              >
                {logout.isPending ? (
                  <SpinnerIcon className="size-3.5 animate-spin" />
                ) : (
                  <LeaveIcon className="size-3.5" />
                )}
                {t("settings.account.logOut")}
              </Button>
            }
          />
          <DeleteAccountSection handle={dto.name} />
        </SettingsGroup>
      </SettingsSection>
    </SettingsPage>
  );
}

/** Shared markdown font size — drives both `--md-prose-size` and
 *  `--md-code-size`. Thumb glides under the cursor (step="any") while
 *  the *applied* value still snaps to integer steps via Math.round in
 *  the change handler, so the visible 12 / 13 / 14 px ladder is
 *  preserved without the segmented hand-feel of a snapping thumb. */
function SharedFontSizeRow({
  value,
  onChange,
  onReset,
}: {
  value: number | undefined;
  onChange: (value: number) => void;
  onReset: () => void;
}): JSX.Element {
  const sizeValue = value ?? DEFAULT_MARKDOWN_FONT_SIZE;
  const isOverride = typeof value === "number";
  const [dragRaw, setDragRaw] = useState<number | null>(null);
  const thumbValue = dragRaw ?? sizeValue;
  const settleDrag = (): void => setDragRaw(null);
  const stepBy = (delta: number): void => {
    const next = Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, sizeValue + delta));
    if (next !== sizeValue) onChange(next);
    setDragRaw(null);
  };

  return (
    <SettingsRow
      label={t("settings.appearance.fontSize")}
      description={
        isOverride
          ? t("settings.appearance.fontSizeShared")
          : t("settings.appearance.fontSizeDefault")
      }
      htmlFor="markdown-font-size"
      wrapMobile
      control={
        <div className="flex w-full items-center gap-2.5 sm:w-auto">
          <RangeInput
            id="markdown-font-size"
            min={FONT_SIZE_MIN}
            max={FONT_SIZE_MAX}
            step="any"
            value={thumbValue}
            aria-label={t("settings.appearance.fontSizeLabel")}
            onChange={(e) => {
              const raw = Number(e.target.value);
              setDragRaw(raw);
              const rounded = Math.round(raw);
              if (rounded !== sizeValue) onChange(rounded);
            }}
            onPointerUp={settleDrag}
            onPointerCancel={settleDrag}
            onBlur={settleDrag}
            onKeyDown={(e) => {
              switch (e.key) {
                case "ArrowLeft":
                case "ArrowDown":
                  e.preventDefault();
                  stepBy(-1);
                  break;
                case "ArrowRight":
                case "ArrowUp":
                  e.preventDefault();
                  stepBy(1);
                  break;
                case "PageDown":
                  e.preventDefault();
                  stepBy(-2);
                  break;
                case "PageUp":
                  e.preventDefault();
                  stepBy(2);
                  break;
                case "Home":
                  e.preventDefault();
                  if (sizeValue !== FONT_SIZE_MIN) onChange(FONT_SIZE_MIN);
                  setDragRaw(null);
                  break;
                case "End":
                  e.preventDefault();
                  if (sizeValue !== FONT_SIZE_MAX) onChange(FONT_SIZE_MAX);
                  setDragRaw(null);
                  break;
                default:
                  break;
              }
            }}
            className="min-w-0 flex-1 sm:w-36 sm:flex-none"
          />
          <span
            aria-live="polite"
            className={cn(
              "min-w-9 text-right font-mono text-xs font-semibold tabular-nums",
              isOverride ? "text-accent" : "text-text-primary",
            )}
          >
            {sizeValue}px
          </span>
          <button
            type="button"
            onClick={onReset}
            disabled={!isOverride}
            className="inline-flex shrink-0 items-center gap-1 rounded-(--control-radius) px-1.5 py-1 text-xs font-medium text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary focus-outline disabled:cursor-not-allowed disabled:opacity-35 disabled:hover:bg-transparent disabled:hover:text-text-muted"
          >
            <UndoIcon className="size-3" />
            {t("settings.appearance.reset")}
          </button>
        </div>
      }
    />
  );
}

/** Per-slot markdown font *family* row. Font size is shared and
 *  rendered separately by {@link SharedFontSizeRow}; this component
 *  only edits family + previews using the inherited shared size. */
function MarkdownFontRow({
  title,
  fontFamily,
  fontSize,
  presets,
  sample,
  onFontChange,
  onReset,
  code = false,
}: {
  title: string;
  fontFamily: string | undefined;
  fontSize: number | undefined;
  presets: ReadonlyArray<FontPreset>;
  sample: string;
  onFontChange: (value: string) => void;
  onReset: () => void;
  code?: boolean;
}): JSX.Element {
  const fontValue = fontFamily ?? "";
  const previewSize = fontSize ?? DEFAULT_MARKDOWN_FONT_SIZE;
  const inputId = `markdown-${title.toLowerCase()}-font`;
  const sampleStyle: CSSProperties = {
    fontFamily:
      fontValue ||
      (code
        ? "var(--appearance-code-font, var(--font-mono))"
        : "var(--appearance-prose-font, var(--font-sans))"),
    fontSize: `${previewSize}px`,
  };

  return (
    <SettingsRow stack>
      <div className="flex items-center justify-between gap-4">
        <label htmlFor={inputId} className="text-sm font-semibold text-text-primary">
          {title}
        </label>
        <button
          type="button"
          onClick={onReset}
          disabled={!fontValue}
          className="inline-flex shrink-0 items-center gap-1 rounded-(--control-radius) px-1.5 py-1 text-xs font-medium text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary focus-outline disabled:cursor-not-allowed disabled:opacity-35 disabled:hover:bg-transparent disabled:hover:text-text-muted"
        >
          <UndoIcon className="size-3" />
          {t("settings.appearance.reset")}
        </button>
      </div>

      <Input
        id={inputId}
        value={fontValue}
        onChange={(e) => onFontChange(e.target.value)}
        placeholder={t("settings.appearance.themeDefault")}
        spellCheck={false}
        className={cn("h-8 w-full", code && "font-mono text-xs")}
      />

      <div className="flex flex-wrap gap-1.5">
        {presets.map((preset) => {
          const isSelected = fontValue === preset.value;
          return (
            <button
              key={`${title}-${preset.label}`}
              type="button"
              data-settings-pill
              data-selected={isSelected || undefined}
              onClick={() => onFontChange(preset.value)}
              className={cn(
                "max-w-full [overflow-wrap:anywhere] rounded-(--pill-radius) border px-2.5 py-0.5 text-(length:--text-meta) font-medium transition-colors focus-outline",
                isSelected
                  ? "border-accent bg-accent text-text-on-accent"
                  : "border-border bg-bg-floating text-text-secondary hover:border-(--border-control-hover) hover:text-text-primary",
              )}
            >
              {preset.label}
            </button>
          );
        })}
      </div>

      <div
        className={cn(
          "min-w-0 [overflow-wrap:anywhere] rounded-(--control-radius) border border-border-subtle bg-bg-floating px-3 py-2 text-text-primary",
          code && "font-mono",
        )}
        style={sampleStyle}
      >
        {sample}
      </div>
    </SettingsRow>
  );
}
