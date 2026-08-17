import { expect, test, type Page } from "@playwright/test";

async function collectDomHealth(page: Page) {
  return page.evaluate(() => {
    const visible = (el: Element) => {
      const style = window.getComputedStyle(el);
      const rect = el.getBoundingClientRect();
      return (
        style.display !== "none" &&
        style.visibility !== "hidden" &&
        rect.width > 0 &&
        rect.height > 0
      );
    };
    const textOfIdRefs = (el: Element, attr: string) =>
      (el.getAttribute(attr) ?? "")
        .split(/\s+/)
        .map((id) => document.getElementById(id)?.textContent?.trim() ?? "")
        .filter(Boolean)
        .join(" ");
    const labelText = (el: Element) => {
      if (el instanceof HTMLInputElement && ["hidden", "submit", "reset", "button"].includes(el.type)) {
        return "ignored";
      }
      if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) {
        const explicit = [...el.labels].map((label) => label.textContent?.trim() ?? "").join(" ");
        if (explicit.trim()) return explicit;
      }
      const idLabel = el.id
        ? document.querySelector(`label[for="${CSS.escape(el.id)}"]`)?.textContent?.trim() ?? ""
        : "";
      return [
        el.getAttribute("aria-label"),
        textOfIdRefs(el, "aria-labelledby"),
        el.getAttribute("title"),
        el.getAttribute("placeholder"),
        idLabel,
        el.textContent,
      ]
        .map((value) => value?.trim() ?? "")
        .find(Boolean);
    };
    const interactiveSelector = [
      "a[href]",
      "button",
      "input",
      "select",
      "textarea",
      "[role='button']",
      "[role='checkbox']",
      "[role='link']",
      "[role='menuitem']",
      "[role='switch']",
      "[role='tab']",
    ].join(",");
    return {
      path: location.pathname + location.search,
      overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth,
      missingNames: [...document.querySelectorAll("input, textarea, select")]
        .filter((el) => !(el as HTMLInputElement).name)
        .map((el) => ({
          tag: el.tagName,
          type: el.getAttribute("type"),
          placeholder: el.getAttribute("placeholder"),
          aria: el.getAttribute("aria-label"),
        })),
      unlabeledControls: [...document.querySelectorAll(interactiveSelector)]
        .filter((el) => visible(el) && !labelText(el))
        .map((el) => ({
          tag: el.tagName,
          role: el.getAttribute("role"),
          type: el.getAttribute("type"),
          name: (el as HTMLInputElement).name,
          href: el.getAttribute("href"),
          className: el.getAttribute("class"),
        })),
      badLocalLinks: [...document.querySelectorAll<HTMLAnchorElement>("a[href]")]
        .map((a) => a.href)
        .filter((href) => href.startsWith(location.origin) && /[（）]/.test(href)),
    };
  });
}

test("core routes are reachable without form, link, or overflow regressions", async ({
  page,
  request,
}) => {
  const consoleErrors: string[] = [];
  page.on("console", (msg) => {
    if (["error", "warning"].includes(msg.type())) consoleErrors.push(msg.text());
  });

  const workspacesResp = await request.get("/api/workspaces");
  const workspaces = workspacesResp.ok() ? await workspacesResp.json() : [];
  const wsSlug = Array.isArray(workspaces) ? workspaces[0]?.slug : null;

  const routes = [
    "/debug",
    "/terminal",
    "/usage",
    "/files",
    "/tasks",
    "/settings",
    "/settings/general",
    "/settings/appearance",
    "/settings/shortcuts",
    "/settings/models",
    "/settings/plugins",
    "/settings/privacy",
    "/settings/about",
    "/mcp",
    "/cron",
    ...(wsSlug
      ? [`/chat/${wsSlug}/dag`, `/chat/${wsSlug}/ledger`, `/chat/${wsSlug}/replays`]
      : []),
  ];

  for (const route of routes) {
    await page.goto(route);
    await page.waitForLoadState("domcontentloaded");
    await page.waitForTimeout(700);
    const health = await collectDomHealth(page);
    if (route === "/debug") expect(health.path).toMatch(/^\/chat/);
    expect(health.overflowX, `${route} should not overflow horizontally`).toBe(false);
    expect(health.missingNames, `${route} form controls need names`).toEqual([]);
    expect(health.unlabeledControls, `${route} interactive controls need labels`).toEqual([]);
    expect(health.badLocalLinks, `${route} should not contain malformed local links`).toEqual([]);
  }

  expect(
    consoleErrors.filter(
      (line) =>
        !line.includes("Download the React DevTools") &&
        !line.includes("[vite]"),
    ),
  ).toEqual([]);
});

test("terminal page requires an explicit connect action", async ({ page }) => {
  await page.goto("/terminal");
  await expect(page.getByText(/连接本机终端|Connect local terminal/)).toBeVisible();
  await expect(page.getByRole("button", { name: /连接终端|Connect terminal/ })).toBeVisible();
  const wsRequests: string[] = [];
  page.on("websocket", (ws) => wsRequests.push(ws.url()));
  await page.waitForTimeout(500);
  expect(wsRequests.filter((url) => url.includes("/ws/terminal"))).toEqual([]);
});

test("create workspace dialog keeps form controls named and labelled", async ({ page }) => {
  await page.goto("/chat");
  await page.waitForLoadState("domcontentloaded");
  await page.keyboard.press("Control+K");
  const palette = page.getByRole("dialog", {
    name: /搜索命令、跳转、唤醒 agent|Search commands/,
  });
  await palette.getByRole("combobox").fill("workspace");
  const newWorkspaceOption = palette.getByRole("option", {
    name: /新建工作空间|New workspace/,
  });
  await expect(newWorkspaceOption, "command palette should not duplicate New workspace").toHaveCount(1);
  await newWorkspaceOption.first().click();
  const dialog = page.getByRole("dialog", { name: /创建工作空间|Create workspace/ });
  await expect(dialog).toBeVisible();
  // "Add another folder" lives in the collapsed Advanced section — expand it first.
  await dialog
    .getByRole("button", { name: /高级源码上下文|Advanced source context/ })
    .click();
  const addFolder = dialog.getByRole("button", { name: /再加一个文件夹|Add another folder/ });
  await addFolder.scrollIntoViewIfNeeded();
  await addFolder.click();
  const health = await collectDomHealth(page);
  expect(health.missingNames, "create workspace form controls need names").toEqual([]);
  expect(health.unlabeledControls, "create workspace controls need labels").toEqual([]);
  expect(health.overflowX, "create workspace dialog should not overflow horizontally").toBe(false);
});

test("usage pricing table is visible and editable without saving", async ({ page }) => {
  await page.goto("/usage");
  await expect(page.getByRole("heading", { name: /用量 \/ 成本|Usage \/ Cost/ })).toBeVisible();
  await expect(page.getByRole("heading", { name: /价目表|Pricing table/ })).toBeVisible();
  const firstRate = page.locator('input[name^="pricing-"][name$="-input"]').first();
  await expect(firstRate).toBeVisible();
  await firstRate.fill("1.234");
  await expect(page.getByRole("button", { name: /保存|Save/ })).toBeEnabled();
});

test("usage pricing reset requires confirmation", async ({ page }) => {
  let resetCalls = 0;
  await page.route("**/api/usage/pricing", async (route) => {
    if (route.request().method() === "DELETE") resetCalls += 1;
    await route.fallback();
  });
  await page.goto("/usage");
  await expect(page.getByRole("heading", { name: /价目表|Pricing table/ })).toBeVisible();
  await page.getByRole("button", { name: /恢复默认|Reset default/ }).click();
  const dialog = page.getByRole("dialog", {
    name: /恢复内置价目表|Reset to built-in pricing/,
  });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText(/价目表配置|pricing config/);
  await dialog.getByRole("button", { name: /取消|Cancel/ }).click();
  await expect(dialog).toBeHidden();
  expect(resetCalls).toBe(0);
});

test("notification mark-all-read uses app confirmation before changing read state", async ({
  page,
}) => {
  let markReadCalls = 0;
  await page.addInitScript(() => {
    window.localStorage.removeItem("swarmx:notif:read:v1");
  });
  await page.route(/\/api\/message(\?.*)?$/, async (route) => {
    if (route.request().method() !== "GET") return route.fallback();
    await route.fulfill({
      json: Array.from({ length: 6 }, (_, i) => ({
        id: i + 1,
        from_agent: `agent-${i + 1}`,
        to_agent: "user",
        kind: "message",
        body: `message ${i + 1}`,
        sent_at: Date.now() - i * 1000,
        delivered_at: null,
        read_at: null,
        in_reply_to: null,
        meta: null,
      })),
    });
  });
  await page.route("**/api/message/read", async (route) => {
    markReadCalls += 1;
    await route.fulfill({ json: { ok: true } });
  });
  await page.route("**/api/blackboard", async (route) => {
    await route.fulfill({ json: [] });
  });
  await page.route("**/api/workspaces", async (route) => {
    await route.fulfill({ json: [] });
  });
  await page.route("**/api/agent", async (route) => {
    await route.fulfill({ json: [] });
  });

  await page.goto("/notifications");
  await expect(page.getByText(/6 条未读|6 unread/)).toBeVisible();
  await page.getByRole("button", { name: /全部标为已读|Mark all read/ }).click();
  const dialog = page.getByRole("dialog", {
    name: /全部标为已读|Mark all read/,
  });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText(/6/);
  await dialog.getByRole("button", { name: /取消|Cancel/ }).click();
  await expect(dialog).toBeHidden();
  expect(markReadCalls).toBe(0);
});

test("privacy clear local data requires app confirmation", async ({ page }) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("swarmx:test-preserve", "1");
  });

  await page.goto("/settings/privacy");
  await page.getByRole("button", { name: /清空 swarmx:\*|Clear swarmx:\*/ }).click();
  const dialog = page.getByRole("dialog", {
    name: /清空本地数据|Clear local data/,
  });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText(/不可撤销|can't be undone/);
  await dialog.getByRole("button", { name: /取消|Cancel/ }).click();
  await expect(dialog).toBeHidden();
  await expect
    .poll(() => page.evaluate(() => window.localStorage.getItem("swarmx:test-preserve")))
    .toBe("1");
});

test("mcp api-key dialog keeps the secret input named and labelled", async ({ page }) => {
  await page.route("**/api/mcp/env", async (route) => {
    await route.fulfill({
      json: {
        node: { present: true, version: "v22.17.0", adequate: true, minMajor: 18 },
        npm: { present: true, version: "10.9.2" },
        uv: { present: false, version: null },
      },
    });
  });
  await page.route("**/api/mcp/status", async (route) => {
    await route.fulfill({
      json: {
        claude: ["context7"],
        codex: [],
        keys: {
          context7: { present: true, masked: "••••adee", consistent: true },
        },
      },
    });
  });

  await page.goto("/mcp");
  await page.getByRole("button", { name: /修改密钥|Change key/ }).click();
  const dialog = page.getByRole("dialog", {
    name: /修改密钥|Change key|填写 API Key|API Key/,
  });
  await expect(dialog).toBeVisible();
  const input = dialog.getByLabel(/API Key/);
  await expect(input).toHaveAttribute("name", "mcp-api-key");
  const health = await collectDomHealth(page);
  expect(health.missingNames, "mcp key dialog form controls need names").toEqual([]);
  expect(health.unlabeledControls, "mcp key dialog controls need labels").toEqual([]);
});

test("agent drawer does not connect to dead PTY and keeps recording actions separate", async ({
  page,
  request,
}) => {
  const ptyRequests: string[] = [];
  page.on("websocket", (ws) => ptyRequests.push(ws.url()));
  const [usageResp, workspacesResp] = await Promise.all([
    request.get("/api/usage"),
    request.get("/api/workspaces"),
  ]);
  const usage = usageResp.ok() ? await usageResp.json() : null;
  const workspaces = workspacesResp.ok() ? await workspacesResp.json() : [];
  const row = Array.isArray(usage?.by_agent) ? usage.by_agent[0] : null;
  const workspace = Array.isArray(workspaces)
    ? workspaces.find((w) => w.id === row?.workspace_id) ?? workspaces[0]
    : null;
  if (!row || !workspace?.slug) test.skip(true, "no agent usage rows in local data");
  await page.goto(`/chat/${workspace.slug}?agent=${encodeURIComponent(row.agent_id)}`);
  await expect(page.getByRole("dialog", { name: /Agent drawer/ })).toBeVisible();
  const drawer = page.getByRole("dialog", { name: /Agent drawer/ });
  await expect(drawer.getByRole("tab", { name: /录像|Recordings/ })).toBeVisible();
  const downloads = drawer.locator("a[download]");
  if ((await downloads.count()) > 0) {
    await expect(downloads.first()).toBeVisible();
  }
  await page.waitForTimeout(500);
  expect(ptyRequests.filter((url) => url.includes("/ws/pty/"))).toEqual([]);
});
