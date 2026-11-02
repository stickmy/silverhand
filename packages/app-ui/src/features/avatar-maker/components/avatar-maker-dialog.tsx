import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import "./avatar-maker.css";
import { CancelledIcon, RandomizeIcon, UndoIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { cn } from "@/lib/cn";
import { getPlatform } from "@/platform";
import { AVATAR_LIBRARY } from "../model/avatar-library";
import {
  composePartThumbnailSvg,
  defaultAvatarConfig,
  randomAvatarConfig,
  type AvatarConfig,
} from "../model/avatar-config";
import { renderAvatarPng } from "../model/render-avatar-png";
import { AvatarConfigPreview } from "./avatar-config-preview";
import { t } from "@/lib/i18n";

const categoryLabels = (): Record<string, string> => ({
  eyebrows: t("avatarMaker.brows"),
  eyes: t("avatarMaker.eyes"),
  nose: t("avatarMaker.nose"),
  mouth: t("avatarMaker.mouth"),
  hair: t("avatarMaker.hair"),
  glasses: t("avatarMaker.glasses"),
  headwear: t("avatarMaker.headwear"),
  status: t("avatarMaker.mood"),
  gesture: t("avatarMaker.gesture"),
  buddy: t("avatarMaker.buddy"),
  background: t("avatarMaker.color"),
});

const BACKGROUND_TAB = "background";

export interface AvatarMakerResult {
  config: AvatarConfig;
  png: File;
}

export interface AvatarMakerDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Starting point: the saved design when there is one, otherwise a random face. */
  initialConfig: AvatarConfig;
  onSave: (result: AvatarMakerResult) => Promise<void>;
}

export function AvatarMakerDialog({
  open,
  onOpenChange,
  initialConfig,
  onSave,
}: AvatarMakerDialogProps) {
  // Read only while open: closed editors render on every avatar surface, including tests
  // and previews that configure no platform.
  if (open && getPlatform().avatarMaker) {
    return (
      <NativeAvatarMaker
        onOpenChange={onOpenChange}
        initialConfig={initialConfig}
        onSave={onSave}
      />
    );
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        data-avatar-maker
        className="max-w-3xl gap-0 p-0"
        cornerBand="top"
        onOpenAutoFocus={(event) => {
          // Focus the sheet itself: the first tabbable control is Reset, and a ring on it
          // at open reads as a pending action.
          event.preventDefault();
          (event.currentTarget as HTMLElement).focus({ preventScroll: true });
        }}
        aria-describedby={undefined}
      >
        {open ? (
          <AvatarMakerBody
            initialConfig={initialConfig}
            onCancel={() => onOpenChange(false)}
            onSave={async (result) => {
              await onSave(result);
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

/** Hands the edit to the platform's native editor; storage stays with the caller.
 *  Mounted only while open, so one mount is one presentation. */
function NativeAvatarMaker({
  onOpenChange,
  initialConfig,
  onSave,
}: Omit<AvatarMakerDialogProps, "open">) {
  const presented = useRef(false);

  useEffect(() => {
    const maker = getPlatform().avatarMaker;

    // StrictMode re-runs effects on the same mount; the ref keeps it to one sheet.
    if (!maker || presented.current) {
      return;
    }

    presented.current = true;

    void (async () => {
      try {
        const result = await maker.edit({ config: initialConfig });

        if (result) {
          const png = new File([result.pngBytes], "avatar.png", { type: "image/png" });
          await onSave({ config: result.config, png });
        }
      } catch {
        toast.error(t("avatarMaker.couldntSaveTheAvatar"));
      } finally {
        onOpenChange(false);
      }
    })();
  }, [initialConfig, onSave, onOpenChange]);

  return null;
}

function AvatarMakerBody({
  initialConfig,
  onCancel,
  onSave,
}: {
  initialConfig: AvatarConfig;
  onCancel: () => void;
  onSave: (result: AvatarMakerResult) => Promise<void>;
}) {
  const [config, setConfig] = useState(initialConfig);
  const [tab, setTab] = useState(AVATAR_LIBRARY.categories[0]?.id ?? BACKGROUND_TAB);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const category = AVATAR_LIBRARY.categories.find((candidate) => candidate.id === tab);
  const tabs = [...AVATAR_LIBRARY.categories.map((candidate) => candidate.id), BACKGROUND_TAB];

  const selectPart = (index: number) => {
    setConfig((current) => ({ ...current, parts: { ...current.parts, [tab]: index } }));
  };

  const save = async () => {
    setSaving(true);
    setError(null);

    try {
      const png = await renderAvatarPng(config);
      await onSave({ config, png });
    } catch {
      setError(t("avatarMaker.couldntSaveTheAvatar"));
      setSaving(false);
    }
  };

  return (
    <div className="flex min-w-0 flex-col">
      <DialogHeader className="px-5 pt-4 pb-3">
        <DialogTitle>{t("avatarMaker.makeAnAvatar")}</DialogTitle>
      </DialogHeader>

      <div className="avatar-maker-body">
        <div className="avatar-maker-stage">
          <div className="avatar-maker-preview">
            <AvatarConfigPreview config={config} />
          </div>
          <div className="avatar-maker-actions">
            <Button variant="neutral" size="sm" onClick={() => setConfig(randomAvatarConfig())}>
              <RandomizeIcon className="size-3.5" />
              {t("avatarMaker.random")}
            </Button>
            <Button variant="neutral" size="sm" onClick={() => setConfig(defaultAvatarConfig())}>
              <UndoIcon className="size-3.5" />
              {t("avatarMaker.reset")}
            </Button>
          </div>
        </div>

        <div
          role="tablist"
          aria-orientation="vertical"
          aria-label={t("avatarMaker.avatarParts")}
          className="avatar-maker-tabs"
        >
          {tabs.map((id) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={tab === id}
              onClick={() => setTab(id)}
              className={cn(
                "shrink-0 rounded-(--control-radius) px-3 py-1.5 text-left text-sm transition-colors focus-outline",
                tab === id
                  ? "bg-bg-hover font-medium text-text-primary"
                  : "text-text-secondary hover:bg-bg-hover/60 hover:text-text-primary",
              )}
            >
              {categoryLabels()[id] ?? id}
            </button>
          ))}
        </div>

        <div
          role="radiogroup"
          aria-label={categoryLabels()[tab] ?? tab}
          className="avatar-maker-grid"
        >
          {tab === BACKGROUND_TAB
            ? AVATAR_LIBRARY.backgrounds.map((color, index) => (
                <PartTile
                  key={color}
                  label={t("avatarMaker.colorN", { n: index + 1 })}
                  selected={config.bg === index}
                  onSelect={() => setConfig((current) => ({ ...current, bg: index }))}
                >
                  <span
                    className="avatar-maker-swatch size-1/2 rounded-full"
                    style={{ background: color }}
                  />
                </PartTile>
              ))
            : category
              ? [
                  ...(category.optional
                    ? [
                        <PartTile
                          key="none"
                          label={t("avatarMaker.none")}
                          selected={config.parts[tab] === -1}
                          onSelect={() => selectPart(-1)}
                        >
                          <CancelledIcon className="avatar-maker-tile-none size-5" />
                        </PartTile>,
                      ]
                    : []),
                  ...category.parts.map((_, index) => (
                    <PartTile
                      key={index}
                      label={`${categoryLabels()[tab] ?? tab} ${index + 1}`}
                      selected={config.parts[tab] === index}
                      onSelect={() => selectPart(index)}
                    >
                      <PartThumbnail svg={composePartThumbnailSvg(tab, index)} />
                    </PartTile>
                  )),
                ]
              : null}
        </div>
      </div>

      <div className="flex items-center justify-end gap-2 px-5 py-3">
        {error ? (
          <p role="alert" className="mr-auto text-xs text-error">
            {error}
          </p>
        ) : null}
        <Button variant="cancel" onClick={onCancel} disabled={saving}>
          {t("common.cancel")}
        </Button>
        <Button variant="primary" onClick={() => void save()} disabled={saving}>
          {saving ? t("avatarMaker.saving") : t("avatarMaker.useAvatar")}
        </Button>
      </div>
    </div>
  );
}

function PartTile({
  label,
  selected,
  onSelect,
  children,
}: {
  label: string;
  selected: boolean;
  onSelect: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-label={label}
      onClick={onSelect}
      className={cn(
        "avatar-maker-tile grid aspect-square place-items-center overflow-hidden rounded-(--control-radius) p-1.5 transition-colors focus-outline",
      )}
    >
      {children}
    </button>
  );
}

// Parts range from a pair of brow strokes to a full hood, so a fixed category crop
// leaves small parts as specks. Each tile crops to its own drawing instead.
const THUMBNAIL_PADDING = 0.14;
const THUMBNAIL_MIN_SIDE = 28;

function PartThumbnail({ svg }: { svg: string }) {
  const fit = (host: HTMLSpanElement | null) => {
    const element = host?.querySelector("svg");

    if (!element) {
      return;
    }

    const box = element.getBBox();
    const side = Math.max(box.width, box.height, THUMBNAIL_MIN_SIDE) * (1 + THUMBNAIL_PADDING * 2);
    const x = box.x + box.width / 2 - side / 2;
    const y = box.y + box.height / 2 - side / 2;
    element.setAttribute("viewBox", `${x} ${y} ${side} ${side}`);
  };

  return (
    <span
      ref={fit}
      className="size-full [&>svg]:size-full"
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}
