import type { ClientPlatform } from "@silverhand/client";
import type { NativeBridge } from "./bridge";

/** The phone's signed-out screen, drawn natively over the page. */
export function createIOSAuthScreen(
  bridge: NativeBridge,
): NonNullable<ClientPlatform["authScreen"]> {
  return {
    show: (state) => bridge.authScreenShow({ state }),
    hide: () => bridge.authScreenHide(),
    subscribe: async (handler) => {
      const listener = await bridge.addListener("authScreen", handler);
      return () => {
        void listener.remove();
      };
    },
  };
}
