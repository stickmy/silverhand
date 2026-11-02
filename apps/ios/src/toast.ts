import type { ClientPlatform } from "@silverhand/client";
import type { NativeBridge } from "./bridge";

/** Toasts under the native bars, which a web toast can't sit above. */
export function createIOSToast(bridge: NativeBridge): NonNullable<ClientPlatform["toast"]> {
  return {
    show: (notice) => bridge.toastShow(notice),
    dismiss: (id) => bridge.toastDismiss({ id }),
    subscribe: async (handler) => {
      const listener = await bridge.addListener("toast", handler);
      return () => {
        void listener.remove();
      };
    },
  };
}
