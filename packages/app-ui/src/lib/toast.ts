import type { ReactNode } from "react";
import { toast as sonner } from "sonner";
import type { ToastCapability, ToastNotice } from "@silverhand/client";
import { getPlatform } from "@/platform";

// The one way the app raises a toast. A platform that draws its own (iOS, where
// native bars sit above the page and would cover a web toast) takes every call;
// everywhere else sonner renders it. Callers never branch on the platform.

export interface ToastOptions {
  /** A toast with the same id replaces the one showing. */
  id?: string;
  description?: string;
  /** `Infinity` keeps it until dismissed. */
  duration?: number;
  action?: { label: string; onClick: (event: { preventDefault(): void }) => void };
  /** Web only; native draws its own glyph for the tone. */
  icon?: ReactNode;
  /** Native only: a named glyph in place of the tone's. */
  glyph?: ToastNotice["glyph"];
}

type Tone = ToastNotice["tone"];

const actions = new Map<string, ToastOptions["action"]>();
let nextId = 0;
let subscribed: ToastCapability | null = null;

function native(): ToastCapability | null {
  const capability = getPlatform().toast ?? null;
  if (capability && subscribed !== capability) {
    subscribed = capability;
    void capability.subscribe((event) => {
      const action = actions.get(event.id);
      if (!action) {
        return;
      }
      let kept = false;
      action.onClick({ preventDefault: () => (kept = true) });
      if (!kept) {
        actions.delete(event.id);
        void capability.dismiss(event.id);
      }
    });
  }
  return capability;
}

function show(tone: Tone, title: string, options: ToastOptions = {}): string | number {
  const capability = native();
  if (!capability) {
    const { icon, glyph: _glyph, ...rest } = options;
    const web = { ...rest, ...(icon ? { icon } : {}) };
    if (tone === "neutral") {
      return sonner(title, web);
    }
    return sonner[tone](title, web);
  }

  const id = options.id ?? `toast-${++nextId}`;
  if (options.action) {
    actions.set(id, options.action);
  } else {
    actions.delete(id);
  }
  void capability.show({
    id,
    tone,
    glyph: options.glyph,
    title,
    description: options.description,
    sticky: options.duration === Infinity,
    action: options.action ? { label: options.action.label } : undefined,
  });
  return id;
}

export const toast = Object.assign(
  (title: string, options?: ToastOptions) => show("neutral", title, options),
  {
    success: (title: string, options?: ToastOptions) => show("success", title, options),
    error: (title: string, options?: ToastOptions) => show("error", title, options),
    warning: (title: string, options?: ToastOptions) => show("warning", title, options),
    dismiss: (id?: string | number) => {
      const capability = native();
      if (!capability) {
        sonner.dismiss(id);
        return;
      }
      if (id !== undefined) {
        actions.delete(String(id));
      }
      void capability.dismiss(id === undefined ? undefined : String(id));
    },
  },
);
