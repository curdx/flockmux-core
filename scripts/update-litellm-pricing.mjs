#!/usr/bin/env node
// Refresh the embedded LiteLLM pricing snapshot that backs /api/usage.
//
// Runtime also auto-refreshes once per process from the same URL (see
// `spawn_litellm_pricing_refresh` in usage.rs; opt out:
// SWARMX_DISABLE_LITELLM_REFRESH=1). This script keeps the *shipped* embed
// current for offline / first-boot before the background fetch lands.
//
// swarmx can't ask claude/codex for spend, so it scrapes token counts and
// applies a pricing table. The hand-maintained primary rules stay editable;
// this snapshot is the FALLBACK. Source is BerriAI/litellm (same table
// ccusage uses), converted from USD-per-token to USD-per-1M-tokens.
//
// Usage:  node scripts/update-litellm-pricing.mjs            fetch + write
//         node scripts/update-litellm-pricing.mjs --check    assert up to date
//         node scripts/update-litellm-pricing.mjs --from <f> use a local json
import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const SRC =
  "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(ROOT, "crates/swarmx-server/resources/litellm_pricing.json");
const M = 1_000_000;

const isNum = (v) => typeof v === "number" && Number.isFinite(v);
// round to 6 significant-ish decimals so 1.5e-5 * 1e6 lands on a clean 15, not 15.000000002
const r6 = (n) => Math.round(n * 1e6) / 1e6;

function slim(raw) {
  const out = {};
  for (const [name, spec] of Object.entries(raw)) {
    if (name === "sample_spec" || typeof spec !== "object" || spec === null) continue;
    // only models we can actually price (chat/completion); skip embeddings/audio/etc.
    if (!isNum(spec.input_cost_per_token)) continue;
    const e = {
      input: r6(spec.input_cost_per_token * M),
      output: r6((isNum(spec.output_cost_per_token) ? spec.output_cost_per_token : 0) * M),
      cache_read: r6(
        (isNum(spec.cache_read_input_token_cost) ? spec.cache_read_input_token_cost : 0) * M,
      ),
      // LiteLLM's cache_creation == our cache_write; providers without it (openai, gemini) -> 0
      cache_write: r6(
        (isNum(spec.cache_creation_input_token_cost) ? spec.cache_creation_input_token_cost : 0) *
          M,
      ),
    };
    if (isNum(spec.max_input_tokens) && spec.max_input_tokens > 0) {
      e.context_window = Math.trunc(spec.max_input_tokens);
    }
    out[name.toLowerCase()] = e;
  }
  return out;
}

// One model per line, keys sorted — compact bytes but clean git diffs on refresh.
function serialize(obj) {
  const keys = Object.keys(obj).sort();
  const lines = keys.map((k) => `${JSON.stringify(k)}:${JSON.stringify(obj[k])}`);
  return `{\n${lines.join(",\n")}\n}\n`;
}

async function loadRaw() {
  const fromIdx = process.argv.indexOf("--from");
  if (fromIdx !== -1) {
    const file = process.argv[fromIdx + 1];
    if (!file) throw new Error("--from needs a path");
    return JSON.parse(await readFile(file, "utf8"));
  }
  const res = await fetch(SRC);
  if (!res.ok) throw new Error(`fetch ${SRC} -> HTTP ${res.status}`);
  return res.json();
}

const raw = await loadRaw();
const next = serialize(slim(raw));
const count = next.split("\n").length - 2; // minus the { and } lines

if (process.argv.includes("--check")) {
  let current = "";
  try {
    current = await readFile(OUT, "utf8");
  } catch {
    /* missing file = stale */
  }
  if (current !== next) {
    console.error(
      `litellm_pricing.json is stale — run: node scripts/update-litellm-pricing.mjs`,
    );
    process.exit(1);
  }
  console.log(`litellm_pricing.json up to date (${count} models)`);
} else {
  await writeFile(OUT, next);
  console.log(`wrote ${OUT} (${count} models)`);
}
