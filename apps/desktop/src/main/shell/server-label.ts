/** Past this the label is cut in the middle, so both ends stay readable. */
const LABEL_LIMIT = 32;

/**
 * How the menu bar names a remote server: the host, plus the port when it is
 * not the scheme's default. Scheme and path add width without telling two
 * servers apart. A URL that does not parse is shown as typed.
 */
export function serverLabel(url: string): string {
  let host: string;
  try {
    // `host` already omits a default port.
    host = new URL(url).host;
  } catch {
    host = url;
  }

  if (host.length <= LABEL_LIMIT) {
    return host;
  }
  const keep = LABEL_LIMIT - 1;
  const head = Math.ceil(keep / 2);
  return `${host.slice(0, head)}…${host.slice(host.length - (keep - head))}`;
}
