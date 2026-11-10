import { useMemo } from "react";
import { composeAvatarSvg, type AvatarConfig } from "@/features/avatar-maker/model/avatar-config";
import { useLocale } from "@/lib/i18n";
import { authScene } from "./auth-scene-model";
import "./auth-scene.css";

/** The sample conversation drawn in the desktop brand column. */
export function AuthScene(): JSX.Element {
  useLocale();
  const scene = authScene();
  const [first, second] = scene.messages;

  return (
    <div className="auth-scene" aria-hidden>
      <span className="auth-scene-channel">
        <span className="mr-0.5 text-text-muted">#</span>
        {scene.channel}
      </span>
      {first ? <SceneMessage {...first} /> : null}
      {second ? <SceneMessage {...second} indent /> : null}
      <div className="auth-scene-message auth-scene-own">
        <p className="auth-scene-bubble">{scene.own}</p>
      </div>
      <div className="auth-scene-message">
        <SceneFace config={scene.typist} />
        <p className="auth-scene-bubble auth-scene-typing">
          <i />
          <i />
          <i />
        </p>
      </div>
    </div>
  );
}

function SceneMessage({
  name,
  text,
  avatar,
  indent = false,
}: {
  name: string;
  text: string;
  avatar: AvatarConfig;
  indent?: boolean;
}) {
  return (
    <div className="auth-scene-message" data-indent={indent || undefined}>
      <SceneFace config={avatar} />
      <p className="auth-scene-bubble">
        <b>{name}</b>
        {text}
      </p>
    </div>
  );
}

function SceneFace({ config }: { config: AvatarConfig }) {
  const svg = useMemo(() => composeAvatarSvg(config), [config]);

  return (
    <span
      className="auth-scene-face"
      // A fixed, attribute-only SVG composed from the bundled part library.
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}
