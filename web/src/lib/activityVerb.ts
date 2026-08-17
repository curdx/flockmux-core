/**
 * activityVerb — translate a raw `AgentActivity.label` ("Edit src/foo.rs",
 * "Bash npm test") into a user-facing verb phrase ("写 foo.rs", "跑测试").
 *
 * Why this exists (P0-4 of the chat redesign): the member heartbeat row and
 * dispatch card show "what a member is doing right now". The backend emits the
 * raw tool name + arg, which leaks engine internals (Edit/Bash/Grep) and
 * absolute worktree paths into the user-facing chat. This module is the
 * jargon firewall: a PURE function that maps the known tool set to plain
 * verbs and strips path prefixes to a basename. Anything it can't recognise
 * degrades to a neutral "处理中" rather than leaking a raw English tool name.
 *
 * It returns a `{ key, params, fallback }` descriptor (NOT a finished string)
 * so the caller renders it through i18next:
 *   const v = activityVerb(activity.label);
 *   t(v.key, { ...v.params, defaultValue: v.fallback });
 * The `fallback` keeps the row honest even before the `chat.verb.*` locale
 * keys land (i18next's defaultValue), matching the codebase's `t(key, default)`
 * convention.
 */

export interface ActivityVerb {
  /** i18n key under `chat.verb.*`. */
  key: string;
  /** Interpolation params for the key (already path-stripped / cleaned). */
  params: Record<string, string>;
  /** zh fallback used as i18next `defaultValue` when the key is absent. */
  fallback: string;
}

/** Last path segment, with a trailing slash tolerated. Strips absolute /
 *  worktree prefixes so only the filename reaches the user. */
function baseName(arg: string): string {
  const cleaned = arg.trim().replace(/[/\\]+$/, "");
  const seg = cleaned.split(/[/\\]/).pop() ?? cleaned;
  return seg || cleaned;
}

/** Trim a shell command for inline display (first ~36 chars, single line). */
function shortCmd(cmd: string, max = 36): string {
  const one = cmd.replace(/\s+/g, " ").trim();
  return one.length > max ? `${one.slice(0, max - 1)}…` : one;
}

const EDIT_TOOLS = new Set(["edit", "write", "multiedit", "notebookedit", "applypatch", "str_replace"]);
const READ_TOOLS = new Set(["read", "cat", "view"]);
const SEARCH_TOOLS = new Set(["grep", "glob", "ls", "find"]);
const WEB_TOOLS = new Set(["websearch", "webfetch", "fetch"]);

/** Split a label like "Edit src/foo.rs" into ["Edit", "src/foo.rs"]. The arg
 *  may be empty (e.g. a bare "Read"). */
function splitLabel(label: string): { tool: string; arg: string } {
  const trimmed = label.trim();
  const sp = trimmed.indexOf(" ");
  if (sp < 0) return { tool: trimmed, arg: "" };
  return { tool: trimmed.slice(0, sp), arg: trimmed.slice(sp + 1).trim() };
}

function classifyBash(arg: string): ActivityVerb {
  const cmd = arg.toLowerCase();
  // order matters: test/install/build are more specific than a bare command.
  if (/\b(test|jest|vitest|pytest|go test|cargo test|npm (run )?test|pnpm test|yarn test)\b/.test(cmd)) {
    return { key: "chat.verb.test", params: { cmd: shortCmd(arg) }, fallback: "跑测试" };
  }
  if (/\b(npm i\b|npm install|pnpm (i|install|add)|yarn add|yarn install|pip install|cargo add|bundle install|go get)\b/.test(cmd)) {
    return { key: "chat.verb.install", params: {}, fallback: "装依赖" };
  }
  if (/\b(build|cargo build|tsc|vite build|webpack|make\b|rollup|esbuild|next build)\b/.test(cmd)) {
    return { key: "chat.verb.build", params: {}, fallback: "构建" };
  }
  const git = cmd.match(/\bgit\s+([a-z-]+)/);
  if (git) {
    return { key: "chat.verb.git", params: { sub: git[1] }, fallback: `执行 git ${git[1]}` };
  }
  return { key: "chat.verb.run", params: { cmd: shortCmd(arg) }, fallback: `运行 ${shortCmd(arg)}` };
}

/**
 * Map one activity label to a user-facing verb descriptor.
 *
 * @param label  the raw `AgentActivity.label`.
 * @param kind   "tool" | "system" | "watchdog" — system steps that don't match a known tool
 *               degrade to a neutral verb instead of echoing internal phrasing;
 *               "watchdog" is the S5 stuck-watchdog nudge (its label is a full
 *               Chinese sentence the member rail renders as-is).
 */
export function activityVerb(label: string, kind: "tool" | "system" | "watchdog" = "tool"): ActivityVerb {
  if (!label || !label.trim()) {
    return { key: "chat.verb.generic", params: {}, fallback: "处理中" };
  }
  // S5 看门狗唤醒:后端 label 是整句中文(成员条/活动流原样展示);聊天里的
  // "正在做什么"行动词行走 i18n 键,而不是被 jargon-firewall 压成"处理中"。
  if (kind === "watchdog") {
    return { key: "watchdog.nudging", params: {}, fallback: "系统正在唤醒它" };
  }
  const { tool, arg } = splitLabel(label);
  const t = tool.toLowerCase();

  if (EDIT_TOOLS.has(t)) {
    const file = baseName(arg);
    return file
      ? { key: "chat.verb.edit", params: { file }, fallback: `写 ${file}` }
      : { key: "chat.verb.editGeneric", params: {}, fallback: "改文件" };
  }
  if (READ_TOOLS.has(t)) {
    const file = baseName(arg);
    return file
      ? { key: "chat.verb.read", params: { file }, fallback: `读 ${file}` }
      : { key: "chat.verb.readGeneric", params: {}, fallback: "读文件" };
  }
  if (t === "bash" || t === "shell" || t === "run" || t === "execute") {
    return classifyBash(arg);
  }
  if (SEARCH_TOOLS.has(t)) {
    return { key: "chat.verb.search", params: {}, fallback: "查代码" };
  }
  if (WEB_TOOLS.has(t)) {
    return { key: "chat.verb.web", params: {}, fallback: "查资料" };
  }
  if (t === "task" || t === "agent" || t === "dispatch") {
    return { key: "chat.verb.task", params: {}, fallback: "派子任务" };
  }
  if (t === "todowrite" || t === "todo") {
    return { key: "chat.verb.todo", params: {}, fallback: "更新计划" };
  }
  // Swarm MCP tools — the heart of multi-agent coordination, so the trace
  // timeline should name them plainly instead of a generic "处理中". Match on
  // the `swarm_` prefix the MCP server exposes (swarm_send_message,
  // swarm_read_blackboard, swarm_write_blackboard, swarm_list_messages,
  // swarm_spawn_worker, swarm_run_spell, …).
  if (t.startsWith("swarm_")) {
    const op = t.slice("swarm_".length);
    if (op.includes("send") || op.includes("message")) {
      return { key: "chat.verb.swarmMessage", params: {}, fallback: "收发消息" };
    }
    if (op.includes("blackboard")) {
      const write = op.includes("write");
      return write
        ? { key: "chat.verb.swarmWrite", params: {}, fallback: "写黑板" }
        : { key: "chat.verb.swarmRead", params: {}, fallback: "读黑板" };
    }
    if (op.includes("spawn") || op.includes("worker") || op.includes("spell")) {
      return { key: "chat.verb.swarmSpawn", params: {}, fallback: "派成员" };
    }
    return { key: "chat.verb.swarmCoord", params: {}, fallback: "协调 swarm" };
  }

  // Unknown leading token: NEVER echo a raw English tool name (jargon-firewall).
  // A "system"-kind step is a non-tool phase; render the neutral "推进中".
  if (kind === "system") {
    return { key: "chat.verb.advance", params: {}, fallback: "推进中" };
  }
  return { key: "chat.verb.generic", params: {}, fallback: "处理中" };
}
