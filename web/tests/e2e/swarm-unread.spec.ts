/**
 * Real-stack unread projection check — NO page.route / NO WS mocks.
 *
 * Expects the isolated unread stack:
 *   frontend http://127.0.0.1:5201  →  backend http://127.0.0.1:7801
 * Fixture workspace slug `ab27bd62`, agent `claude-unread-real` (seeded).
 *
 * Stays on /fusion so this browser never mounts MessagesPanel
 * (useScrollMarkRead). Close any other client on :5201/:7801 first —
 * Cursor Simple Browser on /chat will steal read_at in ~400ms.
 *
 * Run one project at a time against this shared stack (workers=1).
 */
import { expect, test } from "@playwright/test";

const FRONTEND = process.env.PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:5201";
const BACKEND = process.env.SWARMX_TEST_BACKEND ?? "http://127.0.0.1:7801";
const WS_SLUG = process.env.SWARMX_UNREAD_SLUG ?? "ab27bd62";
const AGENT = process.env.SWARMX_UNREAD_AGENT ?? "claude-unread-real";

async function postUnread(body: string) {
  const res = await fetch(`${BACKEND}/api/message`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      from: AGENT,
      to: "user",
      kind: "reply",
      body,
    }),
  });
  if (!res.ok) throw new Error(`POST /api/message → ${res.status}`);
  return (await res.json()) as { id: number; read_at: number | null };
}

async function getMessage(id: number) {
  const res = await fetch(`${BACKEND}/api/message?limit=50`);
  const rows = (await res.json()) as Array<{ id: number; read_at: number | null }>;
  return rows.find((m) => m.id === id) ?? null;
}

async function markAllUserRead() {
  const res = await fetch(`${BACKEND}/api/message?limit=200`);
  const rows = (await res.json()) as Array<{
    id: number;
    to_agent: string;
    read_at: number | null;
  }>;
  const ids = rows
    .filter((m) => m.to_agent === "user" && m.read_at == null)
    .map((m) => m.id);
  if (ids.length === 0) return;
  await fetch(`${BACKEND}/api/message/read`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ to: "user", ids }),
  });
}

test.use({ baseURL: FRONTEND });

test.describe.configure({ mode: "serial" });

test("real fusion toolbar: live WS +1, reload stays 1, second id → 2", async ({
  page,
}) => {
  test.setTimeout(60_000);
  await markAllUserRead();

  await page.goto(`/chat/${WS_SLUG}/fusion`);
  await expect(page.getByRole("heading", { name: "模型竞赛" })).toBeVisible();

  const toolbarUnread = (n: number) =>
    page.getByText(new RegExp(`^${n} (未读|unread)$`)).filter({ visible: true });

  const m1 = await postUnread(`pw-live-${Date.now()}`);
  await expect(toolbarUnread(1).first()).toBeVisible({ timeout: 5_000 });

  // Guard: if another client (Cursor chat) marks read, fail loudly.
  const mid = await getMessage(m1.id);
  expect(
    mid?.read_at,
    "message was marked read by another client — close Cursor/chat on this stack",
  ).toBeNull();

  await page.reload();
  await expect(page.getByRole("heading", { name: "模型竞赛" })).toBeVisible();
  await expect(toolbarUnread(1).first()).toBeVisible({ timeout: 5_000 });
  const afterReload = await getMessage(m1.id);
  expect(afterReload?.read_at).toBeNull();
  // Same id must not become 2 after REST hydrate.
  await expect(toolbarUnread(2)).toHaveCount(0);

  const m2 = await postUnread(`pw-second-${Date.now()}`);
  await expect(toolbarUnread(2).first()).toBeVisible({ timeout: 5_000 });
  expect((await getMessage(m2.id))?.read_at).toBeNull();
});
