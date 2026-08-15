# 测试覆盖对照表

把「人测用例」和「已有自动化」对齐，避免假安全感：绿 CI ≠ 产品能用。

## 自动化现状（诚实）

| 层 | 位置 | 实际覆盖 | 不覆盖 |
|---|---|---|---|
| Rust 单测 | `cargo test --workspace` | 存储、协议、wake 协调、delivery 单元、路由部分 | 真 CLI、真 PTY、浏览器路径 |
| 前端单测 | `web` vitest | `needsYou` / `engineFallback` / workspace helpers 等 | 完整页面交互 |
| Playwright e2e | `npm run test:e2e` | 多为 a11y / 烟 / 静态壳 | **几乎没有**真 spawn、真 MCP、真 Consult |
| harness-check | `scripts/harness-check.mjs` | 跨文件不变量（sidecar 清单等） | 运行时行为 |
| directions-smoke | CI 隔离后端 | 方向/API 烟 | UI |
| golden-cli | `scripts/golden-cli-test.sh`（手动） | 真 CLI 登录在 PATH 时 | 默认 CI 不跑 |

**结论：** P0 用户旅程（新建空间 → 说话 → reply → **spawn_worker/handoff** → Consult）**今天几乎全靠人手**。不要用「e2e 绿了」跳过 TC-C01/C02/**L01–L03**/M01。

**2026-08 自审：** 初版 SUITE（仅 A–K）是路由可达清单，**漏了** handoff、附件、引擎特异投递、Goal evidence、merge 冲突、reaper。已补 L/M 与 A′–J′ 加深段。

---

## 用例 ↔ 自动化映射

| 用例域 | 主要 TC | 自动化有没有 | 缺口 |
|---|---|---|---|
| A 冷启动 | A01–A12 | 弱（部分空态单测） | Welcome/CreateWizard/装包 init **无人测** |
| B 空间/方向 | B01–B10 | directions-smoke 部分 | UI 切方向、合并冲突 |
| C 聊天 | C01–C15 | 几乎无 | **最大缺口**；reply/wake/needsYou |
| D 高级 | D01–D11 | 无 | Advanced disclosure、Consult、Fusion |
| E 抽屉 | E01–E06 | 无 | 死 agent 禁 PTY（高风险） |
| F 侧栏 | F01–F11 | 无 | Files jail、Terminal 按需连 |
| G 设置 | G01–G09 | 无 | Plugins 四态、脏离开 |
| H 失败 | H01–H10 | 部分 Rust | billing toast、WS 恢复需人 |
| I 多引擎 | I01–I14 | golden-cli 手动 | 每引擎队长/worker |
| J 装包 | J01–J05 | harness 清单 | **装包后零命令路径必须人** |
| K 架构锁 | K01–K04 | 部分 `input_delivery` / wake 单测 | 双烧回合需真机 |
| L Handoff/多 agent | L01–L10 | **几乎无** | **最大缺口**；CI 碰不到真派活 |
| M MCP/wake-check | M01–M08 | mcp 单测部分 | Stop hook / kimi / opencode JS 需真机 |
| N 用量/价目 | N01–N15 | usage.rs 单测部分 | matching 代际、订阅文案、引擎采集矩阵、CI `--check` 未接线 |
| 附件/optimize | C16–C18 | 无 | 用户高频 |
| 引擎特异 | I15–I21 | 无 | opencode TUI / reasonix SSE / zulu |

价目决策：[usage-pricing-vs-cc-switch-2026-08.md](../research/usage-pricing-vs-cc-switch-2026-08.md) — **先修 primary matching，再刷 LiteLLM；勿默默 runtime 拉网。**

---

## 建议补自动化的优先级（别一次写完）

1. **P0：** Playwright「假后端」：CreateWizard 提交防重入、Advanced tabs localStorage、死 agent 不连 PTY（mock WS）。
2. **P0：** 已有 `input_delivery` / wake 单测保持红线；reasonix 单入口回归（TC-K01）。
3. **P1：** directions-smoke 扩「删除方向 kill agent」。
4. **不要优先：** 全真 CLI e2e 进 CI（贵、脆、要登录）——继续 golden-cli 手动 + SUITE 人手。

---

## 跑一轮最小集（给未来的你）

```text
1. bash scripts/test-stack.sh   # 或常驻 :7788/:5188
2. 清 localStorage advancedTabs
3. 勾 SUITE 全部 P0
4. 本周改过的域勾相关 P1
5. 不爽写进 UX-BACKLOG.md
6. FAIL 开 issue，链 TC-id
```

装包轮另加 TC-J*；发版前不可跳。
