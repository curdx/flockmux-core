import { describe, it, expect } from "vitest";
import { activityVerb } from "./activityVerb";

describe("activityVerb — swarm MCP tools", () => {
  it("names swarm messaging instead of generic 处理中", () => {
    expect(activityVerb("swarm_send_message").fallback).toBe("收发消息");
    expect(activityVerb("swarm_list_messages").fallback).toBe("收发消息");
  });

  it("distinguishes blackboard read vs write", () => {
    expect(activityVerb("swarm_read_blackboard key").fallback).toBe("读黑板");
    expect(activityVerb("swarm_write_blackboard key").fallback).toBe("写黑板");
  });

  it("names worker/spell spawning", () => {
    expect(activityVerb("swarm_spawn_worker").fallback).toBe("派成员");
    expect(activityVerb("swarm_run_spell").fallback).toBe("派成员");
  });

  it("falls back to a generic swarm-coordination verb for other swarm_ tools", () => {
    expect(activityVerb("swarm_something_new").fallback).toBe("协调 swarm");
  });

  it("still handles non-swarm tools (regression guard)", () => {
    expect(activityVerb("bash ls -la").fallback).toContain("ls");
    expect(activityVerb("").fallback).toBe("处理中");
  });

  it("renders a watchdog nudge as 系统正在唤醒它, not 处理中", () => {
    const v = activityVerb("系统正在唤醒它（第 1/2 次）：…", "watchdog");
    expect(v.key).toBe("watchdog.nudging");
    expect(v.fallback).toBe("系统正在唤醒它");
  });
});
