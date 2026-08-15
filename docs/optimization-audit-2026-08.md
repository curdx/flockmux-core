# 优化审计落地纪要 — 2026-08-14

对照计划 `swarmx 优化审计与优先队列` 的执行结果。

## 已完成

| 项 | 结果 |
|---|---|
| P0 文档去腐 | skill 去掉已删 `acp.rs`；`architecture.md` 迁移 → `0029`；深审 §四 H/A′/#16 closure |
| P0 reaper / Wake 证伪 | 单测 `sweep_evicts_exited_slot_after_grace`、`kick_lock_try_lock_*`、`delivery_sem_caps_*` 通过；隔离栈日志确认 `wake coordinator` + `liveness reaper` 启动 |
| P1 reasonix/zulu MCP 身份 | 对齐 kimi：MCP 文件不再嵌入 `--agent-id`/`SWARMX_*`，靠 spawn 进程 env |
| P1 LiveDelivery seam | 新模块 [`input_delivery.rs`](../crates/swarmx-server/src/input_delivery.rs)；bootstrap + wake kick 共用 classify |
| P2 Codex app-server | 仅设计边界：[`docs/design/codex-app-server-opt-in.md`](design/codex-app-server-opt-in.md)；桌面栈对比见 [`docs/research/codex-desktop-vs-swarmx-2026-08.md`](research/codex-desktop-vs-swarmx-2026-08.md) |
| 试用 | 隔离栈 `:7788`/`:5188`；UI 打开「审计试用空间」；引擎探测 claude/codex/kimi/zulu 可用；未实际 spawn Claude（避免烧订阅） |

## 明确不做（仍成立）

- 默认 Claude 不切 ACP / Agent SDK / `-p`
- 不用 A2A 替换 mailbox
- OpenCode 不整迁 ACP（TUI HTTP 仍成立）
- Claude Channels 不进主路径

## 仍开零散债

见 [`deep-review-2026-07-09.md`](deep-review-2026-07-09.md) §四末尾列表（#6/#7/#12/#14/#19–#22）。
