import { randomAvatarConfig } from "@/features/avatar-maker/model/avatar-config";
import { t } from "@/lib/i18n";

/** A fixed face, so the scene reads the same on every launch and platform. */
function sceneFace(seed: number) {
  let state = seed;

  return randomAvatarConfig(undefined, () => {
    state = (state * 16807) % 2147483647;
    return state / 2147483647;
  });
}

const FACES = { leo: sceneFace(11), doge: sceneFace(29), typist: sceneFace(53) };

/** The sign-in pages' sample conversation: the product shows itself before the form. */
export function authScene() {
  return {
    channel: "launch",
    messages: [
      { name: "Leo", text: t("appShell.welcome.leoSays"), avatar: FACES.leo },
      { name: "Doge", text: t("appShell.welcome.dogeSays"), avatar: FACES.doge },
    ],
    own: t("appShell.welcome.youSay"),
    typist: FACES.typist,
  };
}
