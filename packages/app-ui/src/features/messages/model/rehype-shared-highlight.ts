import rehypeHighlight from "rehype-highlight";

type Transformer = ReturnType<typeof rehypeHighlight>;

let shared: Transformer | null = null;

/**
 * `rehype-highlight` with one highlighter for the whole app. The stock plugin
 * builds a highlighter and registers every common grammar each time it is
 * attached, and react-markdown attaches it once per rendered message. The
 * transform keeps no state between files, so one instance serves them all.
 */
export function rehypeSharedHighlight(): Transformer {
  shared ??= rehypeHighlight();
  return shared;
}
