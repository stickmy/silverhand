import { type SVGProps, useId } from "react";

// Each icon renders into the area provided by its parent. Width/height
// come from `<AgentAvatar>` via inline style; viewBox is fixed per icon
// to match the source path coordinate space.

export function ClaudeIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  return (
    <svg viewBox="0 0 125 125" xmlns="http://www.w3.org/2000/svg" {...props}>
      <path
        fill="#d97757"
        d="M54.375 118.75L56.125 111L58.125 101L59.75 93L61.25 83.125L62.125 79.875L62 79.625L61.375 79.75L53.875 90L42.5 105.375L33.5 114.875L31.375 115.75L27.625 113.875L28 110.375L30.125 107.375L42.5 91.5L50 81.625L54.875 76L54.75 75.25H54.5L21.5 96.75L15.625 97.5L13 95.125L13.375 91.25L14.625 90L24.5 83.125L49.125 69.375L49.5 68.125L49.125 67.5H47.875L43.75 67.25L29.75 66.875L17.625 66.375L5.75 65.75L2.75 65.125L0 61.375L0.25 59.5L2.75 57.875L6.375 58.125L14.25 58.75L26.125 59.5L34.75 60L47.5 61.375H49.5L49.75 60.5L49.125 60L48.625 59.5L36.25 51.25L23 42.5L16 37.375L12.25 34.75L10.375 32.375L9.625 27.125L13 23.375L17.625 23.75L18.75 24L23.375 27.625L33.25 35.25L46.25 44.875L48.125 46.375L49 45.875V45.5L48.125 44.125L41.125 31.375L33.625 18.375L30.25 13L29.375 9.75C29.0417 8.625 28.875 7.375 28.875 6L32.75 0.750006L34.875 0L40.125 0.750006L42.25 2.625L45.5 10L50.625 21.625L58.75 37.375L61.125 42.125L62.375 46.375L62.875 47.75H63.75V47L64.375 38L65.625 27.125L66.875 13.125L67.25 9.125L69.25 4.375L73.125 1.87501L76.125 3.25L78.625 6.875L78.25 9.125L76.875 18.75L73.875 33.875L72 44.125H73.125L74.375 42.75L79.5 36L88.125 25.25L91.875 21L96.375 16.25L99.25 14H104.625L108.5 19.875L106.75 26L101.25 33L96.625 38.875L90 47.75L86 54.875L86.375 55.375H87.25L102.125 52.125L110.25 50.75L119.75 49.125L124.125 51.125L124.625 53.125L122.875 57.375L112.625 59.875L100.625 62.25L82.75 66.5L82.5 66.625L82.75 67L90.75 67.75L94.25 68H102.75L118.5 69.125L122.625 71.875L125 75.125L124.625 77.75L118.25 80.875L109.75 78.875L89.75 74.125L83 72.5H82V73L87.75 78.625L98.125 88L111.25 100.125L111.875 103.125L110.25 105.625L108.5 105.375L97 96.625L92.5 92.75L82.5 84.375H81.875V85.25L84.125 88.625L96.375 107L97 112.625L96.125 114.375L92.875 115.5L89.5 114.875L82.25 104.875L74.875 93.5L68.875 83.375L68.25 83.875L64.625 121.625L63 123.5L59.25 125L56.125 122.625L54.375 118.75Z"
      />
    </svg>
  );
}

export function CodexIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  return (
    <svg viewBox="0 0 180 180" xmlns="http://www.w3.org/2000/svg" {...props}>
      <path
        fill="currentColor"
        d="M101.228 164.247C96.2776 164.247 91.5751 163.307 87.1201 161.426C82.6651 159.545 78.7051 156.921 75.2401 153.555C71.4781 154.842 67.5676 155.486 63.5086 155.486C56.8756 155.486 50.7376 153.852 45.0946 150.585C39.4516 147.318 34.8976 142.863 31.4326 137.22C28.0666 131.577 26.3836 125.291 26.3836 118.361C26.3836 115.49 26.7796 112.371 27.5716 109.005C23.6116 105.342 20.5426 101.135 18.3646 96.3828C16.1866 91.5318 15.0976 86.4828 15.0976 81.2358C15.0976 75.8898 16.2361 70.7418 18.5131 65.7918C20.7901 60.8418 23.9581 56.5848 28.0171 53.0208C32.1751 49.3578 36.9766 46.8333 42.4216 45.4473C43.5106 39.8043 45.7876 34.7553 49.2526 30.3003C52.8166 25.7463 57.1726 22.1823 62.3206 19.6083C67.4686 17.0343 72.9631 15.7473 78.8041 15.7473C83.7541 15.7473 88.4566 16.6878 92.9116 18.5688C97.3666 20.4498 101.327 23.0733 104.792 26.4393C108.554 25.1523 112.464 24.5088 116.523 24.5088C123.156 24.5088 129.294 26.1423 134.937 29.4093C140.58 32.6763 145.085 37.1313 148.451 42.7743C151.916 48.4173 153.648 54.7038 153.648 61.6338C153.648 64.5048 153.252 67.6233 152.46 70.9893C156.42 74.6523 159.489 78.9093 161.667 83.7603C163.845 88.5123 164.934 93.5118 164.934 98.7588C164.934 104.105 163.796 109.253 161.519 114.203C159.242 119.153 156.024 123.459 151.866 127.122C147.807 130.686 143.055 133.161 137.61 134.547C136.521 140.19 134.195 145.239 130.631 149.694C127.166 154.248 122.859 157.812 117.711 160.386C112.563 162.96 107.069 164.247 101.228 164.247ZM64.5481 145.685C69.4981 145.685 73.8046 144.645 77.4676 142.566L105.386 126.528C106.376 125.835 106.871 124.895 106.871 123.707V110.936L70.9336 131.577C68.7556 132.864 66.5776 132.864 64.3996 131.577L36.3331 115.391C36.3331 115.688 36.2836 116.034 36.1846 116.43C36.1846 116.826 36.1846 117.42 36.1846 118.212C36.1846 123.261 37.3726 127.914 39.7486 132.171C42.2236 136.329 45.6391 139.596 49.9951 141.972C54.3511 144.447 59.2021 145.685 64.5481 145.685ZM66.0331 121.479C66.6271 121.776 67.1716 121.925 67.6666 121.925C68.1616 121.925 68.6566 121.776 69.1516 121.479L80.2891 115.094L44.5006 94.3038C42.3226 93.0168 41.2336 91.0863 41.2336 88.5123V56.2878C36.2836 58.4658 32.3236 61.8318 29.3536 66.3858C26.3836 70.8408 24.8986 75.7908 24.8986 81.2358C24.8986 86.0868 26.1361 90.7398 28.6111 95.1948C31.0861 99.6498 34.3036 103.016 38.2636 105.293L66.0331 121.479ZM101.228 154.446C106.475 154.446 111.227 153.258 115.484 150.882C119.741 148.506 123.107 145.239 125.582 141.081C128.057 136.923 129.294 132.27 129.294 127.122V95.0463C129.294 93.8583 128.799 92.9673 127.809 92.3733L116.523 85.8393V127.271C116.523 129.845 115.434 131.775 113.256 133.062L85.1896 149.249C90.0406 152.714 95.3866 154.446 101.228 154.446ZM106.871 100.095V79.8993L90.09 70.3953L73.1611 79.8993V100.095L90.09 109.599L106.871 100.095ZM63.5086 52.7238C63.5086 50.1498 64.5976 48.2193 66.7756 46.9323L94.8421 30.7458C89.9911 27.2808 84.6451 25.5483 78.8041 25.5483C73.5571 25.5483 68.8051 26.7363 64.5481 29.1123C60.2911 31.4883 56.9251 34.7553 54.4501 38.9133C52.0741 43.0713 50.8861 47.7243 50.8861 52.8723V84.7998C50.8861 85.9878 51.3811 86.9283 52.3711 87.6213L63.5086 94.1553V52.7238ZM138.947 123.707C143.897 121.529 147.807 118.163 150.678 113.609C153.648 109.055 155.133 104.105 155.133 98.7588C155.133 93.9078 153.896 89.2548 151.421 84.7998C148.946 80.3448 145.728 76.9788 141.768 74.7018L113.999 58.6638C113.405 58.2678 112.86 58.1193 112.365 58.2183C111.87 58.2183 111.375 58.3668 110.88 58.6638L99.7426 64.9008L135.68 85.8393C136.769 86.4333 137.561 87.2253 138.056 88.2153C138.65 89.1063 138.947 90.1953 138.947 91.4823V123.707ZM109.098 48.2688C111.276 46.8828 113.454 46.8828 115.632 48.2688L143.847 64.7523C143.847 64.0593 143.847 63.1683 143.847 62.0793C143.847 57.3273 142.659 52.8228 140.283 48.5658C138.006 44.2098 134.69 40.7448 130.334 38.1708C126.077 35.5968 121.127 34.3098 115.484 34.3098C110.534 34.3098 106.227 35.3493 102.564 37.4283L74.6461 53.4663C73.6561 54.1593 73.1611 55.0998 73.1611 56.2878V69.0588L109.098 48.2688Z"
      />
    </svg>
  );
}

export function CursorIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  return (
    <svg viewBox="0 0 500 545" xmlns="http://www.w3.org/2000/svg" {...props}>
      <path
        fill="currentColor"
        d="m466.383 137.073-206.469-119.2034c-6.63-3.8287-14.811-3.8287-21.441 0l-206.4586 119.2034c-5.5734 3.218-9.0144 9.169-9.0144 15.615v240.375c0 6.436 3.441 12.397 9.0144 15.615l206.4686 119.203c6.63 3.829 14.811 3.829 21.441 0l206.468-119.203c5.574-3.218 9.015-9.17 9.015-15.615v-240.375c0-6.436-3.441-12.397-9.015-15.615zm-12.969 25.25-199.316 345.223c-1.347 2.326-4.904 1.376-4.904-1.319v-226.048c0-4.517-2.414-8.695-6.33-10.963l-195.7577-113.019c-2.3263-1.347-1.3764-4.905 1.3182-4.905h398.6305c5.661 0 9.199 6.136 6.368 11.041h-.009z"
      />
    </svg>
  );
}

export function GeminiIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  // Placeholder — Gemini's official mark isn't bundled. Render a
  // 4-pointed star reminiscent of the Gemini "spark" so the slot
  // doesn't fall back to a letter monogram.
  return (
    <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" {...props}>
      <path
        fill="currentColor"
        d="M12 2c.45 5.5 4.05 9.55 9.5 10-5.45.45-9.05 4.5-9.5 10-.45-5.5-4.05-9.55-9.5-10 5.45-.45 9.05-4.5 9.5-10z"
      />
    </svg>
  );
}

/**
 * OpenCode's official brand mark. It carries its own dark square background, so
 * render it as a full-bleed raster logo rather than a tinted monochrome badge.
 */
export const OPENCODE_LOGO_DATA_URI =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAYAAADimHc4AAABzUlEQVR4AeycQQrCQBAEF1+g6J/0o5LPCXmCnnNx0E2nNqGEPciw024VffV0PZ/fHo7BqflBCSgAxd+aAhQAE4DjbYACYAJwvA1QAEwAjrcBCgAIDBRpA2AZClAATACOtwEKgAnA8TZAATABOD7egNc8tz2ftJ+4gPQD9r5fAbDBDQXALx00XgGwGAUoACYAx9sABcAE4HgboACYABxvAxSwJHC7XFryLNP4bzYg7KBar4CKUHiugDDgar0CKkLhuQLCgKv1CqgIhecKCAOu1iugIhSeKyAMuFqvgIpQeK6AMOBq/XAC7o9H6z5fdlRAtp4PJ2BrAHSeAmADClAATACOtwEKgAnA8TZAATABON4GKAAmAMev2AD4JTuNVwAsTgEKgAnA8TZAATABON4GKAAmAMfbAAXABOB4G9ApoPe6AnoJdt5XQCfA3uvDCXhOU0ueXmBr3x9OwNoPHH2fAmBDClAATACOtwEKgAnA8TZAAX8QONAVGwDLVIACYAJwfLwByf/+2WJ32k9cQPoBe9+vANigAhQAE4DjbYACYAJw/A8NgH/pQeMVAItVgAJgAnC8DVAATACOtwEKgAnA8TZAATABON4GFALS4w8AAAD//x7wkLQAAAAGSURBVAMAKj5LkLSa6SQAAAAASUVORK5CYII=";

/**
 * Qoder's official brand mark. Unlike the other runtime icons (monochrome
 * vector paths tinted via `currentColor`), Qoder ships a full-color raster
 * logo with its own dark rounded-square background — so it's embedded as a
 * data-URI PNG and rendered edge-to-edge by `AgentAvatar` (no badge tint).
 */
export const QODER_LOGO_DATA_URI =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAALQAAAC0CAMAAAAKE/YAAAAAyVBMVEURERMq21z///8hj0Hh4eGmpqYgICLw8PAkqEqIiInDw8QnwlMvLzFNTU4UKhx5eXopzlgedjg+Pj8THhgbXS9qamyXl5jk+uoXRCWv8cIWNyEfgzx66JlbW121tbXy/fUZUCoUKxwkqUpF33DX+OAcaTMYRCZra2wcajPS0tOlpaZS4ns33WZcXF0mtU9f5IRt5o/J9taU7a0aXS4ySDpEcFIltU5Q1XYnwVMfgjy89Myi77giLSYukUt7hn+T4KlF33F8koPI6dKlvz6KAAAD3ElEQVR42u3c13biMBCAYUkIjAsYMB1CSUjfTdvey/s/1BI2ARIEyJLwjM+Z/zIXPl8UWTLGDmMURVEURVEURVEURVEURVGU+xpH7VFwKWUwaseVUh7AceCJ9bwgxu0+KkqhyPPRsiO1GDO7EoidIWRHe8gI2VFRaOW18JhbUugm3+ZrmJ8KKhjWZSlSBs+OhUHAbF+YVYzgzG1hXBtq/bsSFgEt21ZmIHZbWJc52xcukplukufCUTK79S/yhLOyWrZLUrgsG3ZbOC5oHP6KX7jv0Juk48nx3GHXP7eTYzy77z02Gx9y2W64A8++3V3zZb+Hv34eih04GuGvwxO+0acPiM/C3i3fUjjBOdAd1RivKlfxDXRnyPdUu0A20NIv8/3V3a7R9nuIDpoX8FyRLq5EtdBOx9rq6u5ysVXroR2qrU5D//8xNNG8i2EHj1k6dJiAzw5veRtPF82/OPrAYm5eXTBro7mbTSZ2YE6BdrPuFU3R6x9M9NFuhtr06v+cmaEvALdDnxmiQ7jL/ytmiuZ9B98Tmu3dJXP0KdDi4b2+O5AGXQa6WoqZBboGs4kHzAbNpyDLdGSHPoNA+8wO3QRAe4r7GDdp0HUAdKw4SogcLZktupA9WjXQrIYbrZrRLOG40b7qIH3kaOUt0CZudKA8yAA3Wv0d4TFqtKe+QV5DjR4pj9HlqNHKRZrVcaMjB1M6a7RyC0+5tWSOHrmYHVmj1VP6Bjf6yMHakTk6cnAaZo5WbS1VjhytOkABAN22XPHSD7QDtG+JPoZAx3boLodAH9mhQxB0xQpd5yDokg26z2HQ7NIcnXyEQhfN0QMOhW4Zo99xMHRkiu7X4NBsZIauhhwQ3TJCm5vdfGkrDdAWZjfoVnq0jdnR1+MyLdrK7AjdSom2M7t6AKuYCn1R4xjQkUyBHnCOAq35PoC0nxpOn89reHroOud40OxcQy2bIUeF1nkLscM5MrTGm8AI0fMPuV4O0azke/lD72GPkaJ3v8qMFr3rjLzGi97+XxDuMKO3sR9wo9Xse+xo1T9EGONHK3ab2xygN5btXh7Qr9mdfKAf2WvqYU7QLzbJTm7Q84vtwOFQZ4VeLdudPKGX7IdcoefsxW5zYouus2x73CQ7tupmxujFsj17o/1QuvKHCWMA7J6mulDtluFnxzP7r446XDyRvsEOEwZT6c/ezzDhcuZW6+uzJOwzsL7/2P1ayIuTLVmxwyqD7P3W9wDCwoYsOS3XNn4XkLoDxepQPt32959OGYqS7uDz2lpWHkwSlouS/qQ5b3I2ZRRFURRFURRFURRFURRFURT11D+KvFuepijo0gAAAABJRU5ErkJggg==";

export function PiIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  return (
    <svg viewBox="0 0 800 800" aria-hidden="true" focusable="false" {...props}>
      <path
        fill="currentColor"
        fillRule="evenodd"
        d="M165.29 165.29H517.36V400H400V517.36H282.65V634.72H165.29ZM282.65 282.65V400H400V282.65Z"
      />
      <path fill="currentColor" d="M517.36 400H634.72V634.72H517.36Z" />
    </svg>
  );
}

export function GrokIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  return (
    <svg
      fill="currentColor"
      fillRule="evenodd"
      viewBox="0 0 24 24"
      xmlns="http://www.w3.org/2000/svg"
      {...props}
    >
      <path d="M9.27 15.29l7.978-5.897c.391-.29.95-.177 1.137.272.98 2.369.542 5.215-1.41 7.169-1.951 1.954-4.667 2.382-7.149 1.406l-2.711 1.257c3.889 2.661 8.611 2.003 11.562-.953 2.341-2.344 3.066-5.539 2.388-8.42l.006.007c-.983-4.232.242-5.924 2.75-9.383.06-.082.12-.164.179-.248l-3.301 3.305v-.01L9.267 15.292M7.623 16.723c-2.792-2.67-2.31-6.801.071-9.184 1.761-1.763 4.647-2.483 7.166-1.425l2.705-1.25a7.808 7.808 0 00-1.829-1A8.975 8.975 0 005.984 5.83c-2.533-2.536-3.33-6.436-1.962 9.764 1.022 2.487-.653 4.246-2.34 6.022-.599.63-1.199 1.259-1.682 1.925l7.62-6.815" />
    </svg>
  );
}

/** Antigravity's mark. Gradient ids are per instance so hidden copies don't break visible ones. */
export function AntigravityIcon(props: SVGProps<SVGSVGElement>): JSX.Element {
  const id = useId().replace(/:/g, "");
  return (
    <svg viewBox="13 16 85 82" xmlns="http://www.w3.org/2000/svg" fill="none" {...props}>
      <path
        d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z"
        fill="#3186FF"
      />
      <mask
        id={`${id}m`}
        style={{ maskType: "alpha" }}
        maskUnits="userSpaceOnUse"
        x="13"
        y="18"
        width="85"
        height="78"
      >
        <path
          d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z"
          fill="black"
        />
      </mask>
      <g mask={`url(#${id}m)`}>
        {" "}
        <g filter={`url(#${id}_f0)`}>
          <ellipse
            cx="22.7873"
            cy="26.8098"
            rx="22.7873"
            ry="26.8098"
            transform="matrix(-0.112784 0.99362 -0.99362 -0.112781 66.2473 -15.5344)"
            fill="#FFE432"
          />
        </g>
        <g filter={`url(#${id}_f1)`}>
          <ellipse
            cx="96.491"
            cy="35.1231"
            rx="29.5007"
            ry="30.1492"
            transform="rotate(76.9243 96.491 35.1231)"
            fill="#FC413D"
          />
        </g>
        <g filter={`url(#${id}_f2)`}>
          <ellipse
            cx="9.02988"
            cy="41.6647"
            rx="30.832"
            ry="39.9417"
            transform="rotate(74.1257 9.02988 41.6647)"
            fill="#00B95C"
          />
        </g>
        <g filter={`url(#${id}_f3)`}>
          <ellipse
            cx="11.2212"
            cy="42.8915"
            rx="30.22"
            ry="33.2695"
            transform="rotate(45.6065 11.2212 42.8915)"
            fill="#00B95C"
          />
        </g>
        <g filter={`url(#${id}_f4)`}>
          <ellipse
            cx="75.7546"
            cy="104.822"
            rx="29.0177"
            ry="27.943"
            transform="rotate(76.9243 75.7546 104.822)"
            fill="#3186FF"
          />
        </g>
        <g filter={`url(#${id}_f5)`}>
          <ellipse
            cx="33.5661"
            cy="35.4043"
            rx="33.5661"
            ry="35.4043"
            transform="matrix(-0.409539 0.912293 -0.912294 -0.409537 101.25 -15.1674)"
            fill="#FBBC04"
          />
        </g>
        <g filter={`url(#${id}_f6)`}>
          <path
            d="M2.56802 149.695C-15.8116 142.48 15.5987 83.1163 23.4093 63.2203C31.22 43.3244 52.4514 33.0447 70.831 40.26C89.2107 47.4753 110.996 87.2162 103.185 107.112C95.3742 127.008 20.9477 156.91 2.56802 149.695Z"
            fill="#3186FF"
          />
        </g>
        <g filter={`url(#${id}_f7)`}>
          <path
            d="M113.934 75.8079C109.013 81.5509 96.1724 78.6224 85.253 69.2667C74.3335 59.911 69.4704 47.6711 74.391 41.928C79.3116 36.185 92.1525 39.1136 103.072 48.4692C113.991 57.8249 118.855 70.0648 113.934 75.8079Z"
            fill="#749BFF"
          />
        </g>
        <g filter={`url(#${id}_f8)`}>
          <ellipse
            cx="92.611"
            cy="23.7962"
            rx="44.2411"
            ry="27.5016"
            transform="rotate(34.0763 92.611 23.7962)"
            fill="#FC413D"
          />
        </g>
        <g filter={`url(#${id}_f9)`}>
          <ellipse
            cx="23.4949"
            cy="29.5887"
            rx="23.7071"
            ry="13.7869"
            transform="rotate(112.516 23.4949 29.5887)"
            fill="#FFEE48"
          />
        </g>{" "}
      </g>
      <defs>
        {" "}
        <filter
          id={`${id}_f0`}
          x="2.49348"
          y="-26.5423"
          width="69.0899"
          height="61.2525"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="3.89034" />
        </filter>
        <filter
          id={`${id}_f1`}
          x="28.7524"
          y="-32.0333"
          width="135.477"
          height="134.313"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="18.8078" />
        </filter>
        <filter
          id={`${id}_f2`}
          x="-62.2884"
          y="-21.9253"
          width="142.637"
          height="127.18"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="15.9884" />
        </filter>
        <filter
          id={`${id}_f3`}
          x="-52.5697"
          y="-20.8346"
          width="127.582"
          height="127.452"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="15.9884" />
        </filter>
        <filter
          id={`${id}_f4`}
          x="17.3619"
          y="45.4646"
          width="116.786"
          height="118.715"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="15.1937" />
        </filter>
        <filter
          id={`${id}_f5`}
          x="-7.44765"
          y="-60.4737"
          width="125.303"
          height="122.858"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="13.7698" />
        </filter>
        <filter
          id={`${id}_f6`}
          x="-27.7086"
          y="13.3597"
          width="157.119"
          height="162.029"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="12.297" />
        </filter>
        <filter
          id={`${id}_f7`}
          x="50.4638"
          y="16.981"
          width="87.3973"
          height="83.7738"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="11.0036" />
        </filter>
        <filter
          id={`${id}_f8`}
          x="34.2604"
          y="-28.457"
          width="116.701"
          height="104.506"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="9.29385" />
        </filter>
        <filter
          id={`${id}_f9`}
          x="-15.1522"
          y="-15.9493"
          width="77.2941"
          height="91.076"
          filterUnits="userSpaceOnUse"
          colorInterpolationFilters="sRGB"
        >
          <feGaussianBlur stdDeviation="11.5027" />
        </filter>{" "}
      </defs>
    </svg>
  );
}
