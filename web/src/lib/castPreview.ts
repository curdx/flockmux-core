/**
 * Preview helper for asciicast v2 (.cast) files served at
 * /api/recording/:id.
 *
 * Server doesn't advertise Accept-Ranges, so we can't HTTP-Range the
 * first 16 KB. Instead: stream the response, accumulate ~16 KB of
 * decoded text, then abort the request — the kernel/TCP stack tears
 * the connection down and the rest of the (possibly multi-MB) file is
 * never transferred over the wire.
 *
 * Result is cached process-wide by recording id; the Replays grid can
 * mount + unmount cards freely without re-pulling.
 *
 * Format reminder:
 *   line 0  : JSON header object
 *   line 1+ : `[time_seconds, "o" | "i", "data"]`
 * We only care about the first few "o" (output) frames concatenated
 * into a single string, then ANSI-stripped to plain visible text.
 */

const TARGET_BYTES = 16 * 1024;
const MAX_OUTPUT_CHARS = 1500;
const MAX_LINES = 6;

/** PTY wake injections are instructions TO the agent — noise in a user-facing
 *  thumbnail (same rationale as notifBody dropping wake bodies). */
const WAKE_NOISE =
  /操作员唤醒|请先查收邮箱|请检查共享区|Operator wake|check your (mail|inbox)|swarm_send_message\(to=["']user["']/i;

type Preview = string[];

/** Bump when filter rules change so in-memory thumbs aren't stale across HMR. */
const CACHE_VER = "v2";

const cache = new Map<string, Preview>();
const inflight = new Map<string, Promise<Preview>>();

function cacheKey(id: string): string {
  return `${CACHE_VER}:${id}`;
}

async function fetchHead(url: string): Promise<string> {
  const ctrl = new AbortController();
  let res: Response;
  try {
    res = await fetch(url, { signal: ctrl.signal });
  } catch (e) {
    throw e;
  }
  if (!res.ok || !res.body) {
    ctrl.abort();
    throw new Error(`HTTP ${res.status}`);
  }
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let text = "";
  try {
    while (text.length < TARGET_BYTES) {
      const { value, done } = await reader.read();
      if (done) break;
      text += dec.decode(value, { stream: true });
    }
  } finally {
    // Always abort so the rest of a multi-MB cast isn't pulled over
    // the wire. reader.cancel() also works; AbortController is more
    // portable across fetch implementations.
    ctrl.abort();
  }
  return text;
}

function stripAnsi(s: string): string {
  // Order matters: long sequences first so they don't get half-eaten.
  return s
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, "") // OSC … BEL / ST
    .replace(/\x1b[PX^_][^\x1b]*\x1b\\/g, "") // DCS / SOS / PM / APC
    .replace(/\x1b\[[0-9;?]*[ -/]*[a-zA-Z]/g, "") // CSI
    .replace(/\x1b./g, "") // any remaining ESC + 1
    .replace(/\r/g, "");
}

/** Exported for unit tests — keep wake/injection lines out of thumbs. */
export function filterPreviewLines(rawLines: string[]): Preview {
  return rawLines
    .map((l) => l.trimEnd())
    .filter((l) => l.length > 0 && !WAKE_NOISE.test(l))
    .slice(0, MAX_LINES);
}

function parsePreview(text: string): Preview {
  const lines = text.split("\n");
  if (lines.length < 2) return [];
  let out = "";
  // skip line 0 (header)
  for (let i = 1; i < lines.length; i++) {
    if (!lines[i]) continue;
    try {
      const frame = JSON.parse(lines[i]) as [number, string, string];
      if (frame[1] === "o" && typeof frame[2] === "string") {
        out += frame[2];
        if (out.length >= MAX_OUTPUT_CHARS) break;
      }
    } catch {
      // Partial line at the tail end of our truncated buffer — fine.
      break;
    }
  }
  const stripped = stripAnsi(out).split("\n");
  return filterPreviewLines(stripped);
}

export function getCachedCastPreview(id: string): Preview | undefined {
  return cache.get(cacheKey(id));
}

/** Drop cached thumbs so filter changes apply without a full reload. */
export function clearCastPreviewCache(): void {
  cache.clear();
  inflight.clear();
}

export async function loadCastPreview(url: string, id: string): Promise<Preview> {
  const key = cacheKey(id);
  const cached = cache.get(key);
  if (cached) return cached;
  const ongoing = inflight.get(key);
  if (ongoing) return ongoing;
  const p = (async () => {
    try {
      const text = await fetchHead(url);
      const preview = parsePreview(text);
      cache.set(key, preview);
      return preview;
    } finally {
      inflight.delete(key);
    }
  })();
  inflight.set(key, p);
  return p;
}
