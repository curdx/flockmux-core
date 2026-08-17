import { describe, it, expect } from "vitest";
import { usageIsCollected, uncollectedEngines } from "./usageCoverage";

describe("usageIsCollected", () => {
  it("covers the three flavors transcript.rs tails", () => {
    expect(usageIsCollected("claude")).toBe(true);
    expect(usageIsCollected("codex")).toBe(true);
    expect(usageIsCollected("kimi")).toBe(true);
  });

  it("matches by substring, like the backend flavor check", () => {
    // transcript.rs uses cli.contains(...) — a suffixed plugin id must not
    // silently drop out of coverage on the UI side while the backend tails it.
    expect(usageIsCollected("claude-foo")).toBe(true);
    expect(usageIsCollected("Codex")).toBe(true);
  });

  it("marks uncollected engines as not collected", () => {
    expect(usageIsCollected("opencode")).toBe(false);
    expect(usageIsCollected("reasonix")).toBe(false);
    expect(usageIsCollected("zulu")).toBe(false);
  });

  it("treats null/empty as not collected (never a fake 'covered')", () => {
    expect(usageIsCollected(null)).toBe(false);
    expect(usageIsCollected(undefined)).toBe(false);
    expect(usageIsCollected("")).toBe(false);
  });
});

describe("uncollectedEngines", () => {
  it("dedupes and keeps first-seen order", () => {
    expect(
      uncollectedEngines([
        { cli: "claude" },
        { cli: "zulu" },
        { cli: "opencode" },
        { cli: "zulu" },
      ]),
    ).toEqual(["zulu", "opencode"]);
  });

  it("returns [] when every agent is on a collected engine", () => {
    expect(uncollectedEngines([{ cli: "claude" }, { cli: "kimi" }])).toEqual([]);
    expect(uncollectedEngines([])).toEqual([]);
  });
});
