export type ApiMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface ApiEndpoint {
  name: string;
  method: ApiMethod;
  path: string;
}

type QueryValue = string | number | boolean | null | undefined;

interface MessageListParams {
  to?: string;
  from?: string;
  q?: string;
  limit?: number;
  thread_id?: string;
  only_undelivered?: boolean;
}

function endpoint(name: string, method: ApiMethod, path: string): ApiEndpoint {
  if (!path.startsWith("/api/")) {
    throw new Error(`API endpoint ${name} must stay under /api: ${path}`);
  }
  return { name, method, path };
}

export function queryString(params: Record<string, QueryValue>): string {
  const parts: string[] = [];
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "" || value === false) continue;
    parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  return parts.length ? `?${parts.join("&")}` : "";
}

function encodePathSegments(path: string): string {
  return path.split("/").map(encodeURIComponent).join("/");
}

export const apiRoutes = {
  plugins: {
    list: () => endpoint("plugins.list", "GET", "/api/plugins"),
    /** Kick a background real-usability sweep (actually start each CLI). */
    probe: () => endpoint("plugins.probe", "POST", "/api/plugins/probe"),
    /** Cached probe verdicts + whether a sweep is in flight. */
    probeStatus: () => endpoint("plugins.probeStatus", "GET", "/api/plugins/probe"),
    /** One-click install: streams the whitelisted install command's output (SSE). */
    install: (id: string) =>
      endpoint("plugins.install", "POST", `/api/plugins/${encodeURIComponent(id)}/install`),
  },
  /** Comate Zulu SaaS license (zulu engine credential). */
  comate: {
    get: () => endpoint("comate.get", "GET", "/api/comate"),
    put: () => endpoint("comate.put", "PUT", "/api/comate"),
  },
  /** zulu's model roster under the configured license (one key → N models). */
  zulu: {
    models: () => endpoint("zulu.models", "GET", "/api/zulu/models"),
  },
  mcp: {
    env: () => endpoint("mcp.env", "GET", "/api/mcp/env"),
    status: () => endpoint("mcp.status", "GET", "/api/mcp/status"),
    install: () => endpoint("mcp.install", "POST", "/api/mcp/install"),
    uninstall: () => endpoint("mcp.uninstall", "POST", "/api/mcp/uninstall"),
  },
  agents: {
    list: () => endpoint("agents.list", "GET", "/api/agent"),
    spawn: () => endpoint("agents.spawn", "POST", "/api/agent"),
    kill: (id: string) => endpoint("agents.kill", "DELETE", `/api/agent/${encodeURIComponent(id)}`),
    wake: (id: string) => endpoint("agents.wake", "POST", `/api/agent/${encodeURIComponent(id)}/wake`),
    activity: (id: string) => endpoint("agents.activity", "GET", `/api/agent/${encodeURIComponent(id)}/activity`),
    interrupt: (id: string) => endpoint("agents.interrupt", "POST", `/api/agent/${encodeURIComponent(id)}/interrupt`),
    resume: (id: string) => endpoint("agents.resume", "POST", `/api/agent/${encodeURIComponent(id)}/resume`),
    interruptAll: (workspaceId: string) =>
      endpoint("agents.interruptAll", "POST", `/api/agent/interrupt-all${queryString({ workspace_id: workspaceId })}`),
  },
  usage: {
    summary: (workspaceId?: string) =>
      endpoint("usage.summary", "GET", `/api/usage${queryString({ workspace_id: workspaceId })}`),
    pricing: () => endpoint("usage.pricing", "GET", "/api/usage/pricing"),
    updatePricing: () => endpoint("usage.updatePricing", "PUT", "/api/usage/pricing"),
    resetPricing: () => endpoint("usage.resetPricing", "DELETE", "/api/usage/pricing"),
  },
  goals: {
    list: (workspaceId?: string, threadId?: string | null) =>
      endpoint("goals.list", "GET", `/api/goals${queryString({
        workspace_id: workspaceId,
        thread_id: threadId === null ? "null" : threadId,
      })}`),
    create: () => endpoint("goals.create", "POST", "/api/goals"),
    updateStatus: (id: string) => endpoint("goals.updateStatus", "PATCH", `/api/goals/${encodeURIComponent(id)}/status`),
    evidence: (id: string, limit?: number) =>
      endpoint("goals.evidence", "GET", `/api/goals/${encodeURIComponent(id)}/evidence${queryString({ limit })}`),
    addEvidence: (id: string) => endpoint("goals.addEvidence", "POST", `/api/goals/${encodeURIComponent(id)}/evidence`),
  },
  tasks: {
    list: (workspaceId?: string) =>
      endpoint("tasks.list", "GET", `/api/tasks${queryString({ workspace_id: workspaceId })}`),
    setStatus: (agentId: string) => endpoint("tasks.setStatus", "POST", `/api/tasks/${encodeURIComponent(agentId)}/status`),
  },
  files: {
    list: (dir?: string, workspaceId?: string, all?: boolean) =>
      endpoint("files.list", "GET", `/api/files/list${queryString({ dir, workspace_id: workspaceId, all: all ? "1" : undefined })}`),
    read: (path: string, workspaceId?: string, all?: boolean) =>
      endpoint("files.read", "GET", `/api/files/read${queryString({ path, workspace_id: workspaceId, all: all ? "1" : undefined })}`),
    serve: (path: string) => `/api/file${queryString({ path })}`,
  },
  blackboard: {
    list: () => endpoint("blackboard.list", "GET", "/api/blackboard"),
    read: (path: string) => endpoint("blackboard.read", "GET", `/api/blackboard/${encodePathSegments(path)}`),
    write: (path: string) => endpoint("blackboard.write", "PUT", `/api/blackboard/${encodePathSegments(path)}`),
    history: (path: string, limit = 50, includeContent = false) =>
      endpoint("blackboard.history", "GET", `/api/blackboard-history/${encodePathSegments(path)}${queryString({
        limit,
        include_content: includeContent,
      })}`),
    compact: () => endpoint("blackboard.compact", "POST", "/api/blackboard/compact"),
  },
  cron: {
    list: () => endpoint("cron.list", "GET", "/api/cron"),
    preview: (expr: string, offset: number) =>
      endpoint("cron.preview", "GET", `/api/cron/preview${queryString({ expr, offset })}`),
    create: () => endpoint("cron.create", "POST", "/api/cron"),
    update: (id: string) => endpoint("cron.update", "PUT", `/api/cron/${encodeURIComponent(id)}`),
    delete: (id: string) => endpoint("cron.delete", "DELETE", `/api/cron/${encodeURIComponent(id)}`),
    toggle: (id: string) => endpoint("cron.toggle", "PATCH", `/api/cron/${encodeURIComponent(id)}`),
    run: (id: string) => endpoint("cron.run", "POST", `/api/cron/${encodeURIComponent(id)}/run`),
  },
  messages: {
    list: (params: MessageListParams) => endpoint("messages.list", "GET", `/api/message${queryString({
      to: params.to,
      from: params.from,
      q: params.q,
      limit: params.limit,
      thread_id: params.thread_id,
      only_undelivered: params.only_undelivered,
    })}`),
    send: () => endpoint("messages.send", "POST", "/api/message"),
    markRead: () => endpoint("messages.markRead", "POST", "/api/message/read"),
  },
  recordings: {
    list: (agentId?: string) => endpoint("recordings.list", "GET", `/api/recording${queryString({ agent_id: agentId })}`),
    cast: (id: string) => `/api/recording/${encodeURIComponent(id)}`,
  },
  spells: {
    list: () => endpoint("spells.list", "GET", "/api/spells"),
    run: () => endpoint("spells.run", "POST", "/api/spell/run"),
  },
  workspaces: {
    list: () => endpoint("workspaces.list", "GET", "/api/workspaces"),
    create: () => endpoint("workspaces.create", "POST", "/api/workspaces"),
    delete: (id: string) => endpoint("workspaces.delete", "DELETE", `/api/workspaces/${encodeURIComponent(id)}`),
    roots: (id: string) => endpoint("workspaces.roots", "POST", `/api/workspaces/${encodeURIComponent(id)}/roots`),
    deleteRoot: (id: string, rootId: string) =>
      endpoint("workspaces.deleteRoot", "DELETE", `/api/workspaces/${encodeURIComponent(id)}/roots${queryString({ id: rootId })}`),
    rootSuggestions: (id: string, projectPath?: string) =>
      endpoint("workspaces.rootSuggestions", "GET", `/api/workspaces/${encodeURIComponent(id)}/root-suggestions${queryString({ path: projectPath })}`),
    branches: (id: string) => endpoint("workspaces.branches", "GET", `/api/workspaces/${encodeURIComponent(id)}/branches`),
    /** Workspace budget brake: read the cap + live estimate, set/clear the cap. */
    budget: (id: string) => endpoint("workspaces.budget", "GET", `/api/workspaces/${encodeURIComponent(id)}/budget`),
    setBudget: (id: string) => endpoint("workspaces.setBudget", "PUT", `/api/workspaces/${encodeURIComponent(id)}/budget`),
    threads: (id: string) => endpoint("workspaces.threads", "GET", `/api/workspaces/${encodeURIComponent(id)}/threads`),
    createThread: (id: string) => endpoint("workspaces.createThread", "POST", `/api/workspaces/${encodeURIComponent(id)}/threads`),
    updateThread: (id: string, threadId: string) =>
      endpoint("workspaces.updateThread", "PATCH", `/api/workspaces/${encodeURIComponent(id)}/threads/${encodeURIComponent(threadId)}`),
    setThreadModel: (id: string, threadId: string) =>
      endpoint("workspaces.setThreadModel", "PUT", `/api/workspaces/${encodeURIComponent(id)}/threads/${encodeURIComponent(threadId)}/model`),
    deleteThread: (id: string, threadId: string) =>
      endpoint("workspaces.deleteThread", "DELETE", `/api/workspaces/${encodeURIComponent(id)}/threads/${encodeURIComponent(threadId)}`),
    threadDiff: (id: string, threadId: string) =>
      endpoint("workspaces.threadDiff", "GET", `/api/workspaces/${encodeURIComponent(id)}/threads/${encodeURIComponent(threadId)}/diff`),
    mergeThread: (id: string, threadId: string) =>
      endpoint("workspaces.mergeThread", "POST", `/api/workspaces/${encodeURIComponent(id)}/threads/${encodeURIComponent(threadId)}/merge`),
    fusion: (id: string) =>
      endpoint("workspaces.fusion", "GET", `/api/workspaces/${encodeURIComponent(id)}/fusion`),
    createFusion: (id: string) =>
      endpoint("workspaces.createFusion", "POST", `/api/workspaces/${encodeURIComponent(id)}/fusion`),
    judgeFusion: (id: string, bid: string, auto?: boolean, synthesize?: boolean) =>
      endpoint("workspaces.judgeFusion", "POST", `/api/workspaces/${encodeURIComponent(id)}/fusion/${encodeURIComponent(bid)}/judge${auto ? `?auto=true${synthesize ? "&synthesize=true" : ""}` : ""}`),
    decideFusion: (id: string, bid: string) =>
      endpoint("workspaces.decideFusion", "POST", `/api/workspaces/${encodeURIComponent(id)}/fusion/${encodeURIComponent(bid)}/decide`),
    fusionConsult: (id: string) =>
      endpoint("workspaces.fusionConsult", "POST", `/api/workspaces/${encodeURIComponent(id)}/fusion-consult`),
  },
  prompt: {
    optimize: () => endpoint("prompt.optimize", "POST", "/api/prompt/optimize"),
  },
  attachments: {
    upload: (name?: string) => `/api/attachment${queryString({ name })}`,
  },
  models: {
    get: () => endpoint("models.get", "GET", "/api/models"),
    put: () => endpoint("models.put", "PUT", "/api/models"),
  },
} as const;
