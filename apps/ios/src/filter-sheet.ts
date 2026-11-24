import type { ClientPlatform, FilterSheetState } from "@silverhand/client";
import type { NativeBridge } from "./bridge";
import { rasterizeImage } from "./glyph";

const AVATAR_PX = 28;
const pictures = new Map<string, Promise<string>>();

function picture(url: string): Promise<string> {
  let cached = pictures.get(url);
  if (!cached) {
    cached = rasterizeImage(url, AVATAR_PX);
    pictures.set(url, cached);
  }
  return cached;
}

/** Avatar URLs are the page's object URLs, which native cannot open; it gets
 *  the pictures as PNG bytes instead. */
async function withPictures(state: FilterSheetState): Promise<FilterSheetState> {
  return {
    ...state,
    fields: await Promise.all(
      state.fields.map(async (field) => ({
        ...field,
        options: await Promise.all(
          field.options.map(async (option) =>
            option.avatar?.image
              ? { ...option, avatar: { image: (await picture(option.avatar.image)) || null } }
              : option,
          ),
        ),
      })),
    ),
  };
}

/** A sheet of task filters, drawn natively over the page. */
export function createIOSFilterSheet(
  bridge: NativeBridge,
): NonNullable<ClientPlatform["filterSheet"]> {
  let version = 0;

  return {
    show: async (state) => {
      const current = ++version;
      const native = await withPictures(state);
      if (current === version) {
        await bridge.filterSheetShow({ state: native });
      }
    },
    hide: () => {
      version++;
      return bridge.filterSheetHide();
    },
    subscribe: async (handler) => {
      const listener = await bridge.addListener("filterSheet", handler);
      return () => {
        void listener.remove();
      };
    },
  };
}
