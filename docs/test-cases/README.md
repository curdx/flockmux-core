# swarmx 完整走测用例套件

> **用途：** 人工或 Agent 按条目在真实浏览器里走测 / 调试。  
> **立场：** 把自己当成「刚下载安装包的陌生人」，不是开发机贡献者。  
> **权威性：** 本目录是产品验收清单；`cargo test` / vitest / 现有 e2e **不能**替代这里的 P0 项。

## 怎么用（给未来的 Agent）

1. 起隔离栈：`bash scripts/test-stack.sh`（或装包版）。**禁止**只 curl 就宣称通过。
2. 用 chrome-devtools **真实点 UI**：打开页面 → 点按钮 → 看结果 → 截图留证。
3. 每条用例写结果：`PASS` / `FAIL` / `BLOCKED` / `SKIP`（注明原因）。
4. `FAIL` 必须附：复现步骤、期望 vs 实际、截图或日志路径。
5. 走测中「用得不爽」→ 记入 [UX-BACKLOG.md](./UX-BACKLOG.md)，能当场修的小项直接修，大项开条目。
6. 烧额度的用例（真 spawn Claude / Fusion 全赛 / 多模型会诊）标了 `💰`；默认跑轻量变体，除非用户明确要求全烧。

## 环境矩阵（每轮至少勾一种）

| 代号 | 环境 | 何时必跑 |
|---|---|---|
| E-DEV | `test-stack` :5188/:7788 | 每次功能改动后 |
| E-PKG | 本机安装包（Tauri .app/.dmg 等） | 发版前；任何打包/资源路径改动 |
| E-WIN / E-LIN | Windows / Linux 安装包 | 发版前；涉及路径/`#[cfg]`/sidecar |

## 优先级

| 级 | 含义 |
|---|---|
| **P0** | 陌生人能干活：建空间→说话→reply；**派 worker/handoff**；附件；计费红线可见；**Usage 估算可信（含 opus 代际）** |
| **P1** | 方向/合并冲突、唤醒、多引擎特异投递、Fusion/Consult 分支、Goal evidence、价目编辑 |
| **P2** | 外围页、i18n/a11y、装包桌面细节、LiteLLM CI、Debug |

> **反自欺：** 只验「页面进得去」≠ 完整验收。原 A–K 是壳；**L（handoff）/ M（MCP）** 与各域加深段才是蜂群产品。见 SUITE 末「已知浅勾选」。

## 文件

| 文件 | 内容 |
|---|---|
| [SUITE.md](./SUITE.md) | 全量用例 A–N（含 Handoff/MCP/用量价目） |
| [UX-BACKLOG.md](./UX-BACKLOG.md) | 用户视角不爽点 → 优化候选 |
| [COVERAGE.md](./COVERAGE.md) | 与现有自动化的对照（防重复自信） |

相关决策（写完再开跑）：[usage-pricing-vs-cc-switch-2026-08.md](../research/usage-pricing-vs-cc-switch-2026-08.md)

## 开跑门闩

1. SUITE「产品面 ↔ 域」无空行  
2. 已读价目 research（勿把 BillingSurface 红线当成 Usage 估价）  
3. 本轮最低 P0 含 L/M/N 相关条目  
4. **未写完套件前不开「完整走测」**

## 结果模板（粘到 PR / 会话末尾）

```text
环境: E-DEV | 日期: YYYY-MM-DD | 执行者: …
P0: x/y PASS
P1: …
FAIL:
- TC-… : …
UX 新债:
- …
```
