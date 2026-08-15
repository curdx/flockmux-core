# OpenCode 接入方案（A=PTY / B=ACP）

> 目标：把 opencode 加成 swarmx 的第三个引擎。先 A（PTY，和 claude/codex 同构、复用最多），后 B（ACP，结构化、顺带还清 L4 债并解锁 codex app-server）。
> 本方案的每一步都钉在 claude/codex **现有实现**的对应触点上。所有 opencode 事实均来自源码核对（克隆于 `/Users/wdx/opc/opencode`，v1.17.7）。

---

## 0. 先讲清楚：claude / codex 现在到底怎么跑的

一个 worker 从生到死的全链路（文件:行均为本仓库实测）：

| 阶段 | 机制 | 文件:行 | 是否数据驱动 |
|---|---|---|---|
| **注册** | `cli-plugins/<id>.toml` → `PluginRegistry`；`builtin()` 用 `include_str!` 把 claude/codex 内嵌进二进制（装包后 CWD=`/` 找不到目录也能 spawn）+ 分层 override | `plugins.rs:400-420` | ✅ format 枚举驱动 |
| **写配置（pre-spawn）** | `run_patches` 按 `trust_format`/`mcp_format`/`stop_hook_format` **枚举**派发（不是 `match id`）。claude 写 `~/.claude.json`(trust+mcp local) + `<ws>/.claude/settings.local.json`(Stop hook)；codex 写 per-agent `CODEX_HOME/config.toml`(mcp global, 剥离用户 MCP) + `<ws>/.codex/hooks.json` | `pre_spawn.rs:673` `run_patches` | ✅ 枚举驱动 |
| **argv 拼装** | `[shim, binary, default_args, model_overlay, effort_overlay, (codex `--dangerously-bypass-hook-trust` 探测注入), (claude `--mcp-config <per-agent> --strict-mcp-config`), (claude `--session-id <uuid>`)]` | `spawn.rs:146-348` | 部分（model/effort 模板化；几处 format 判断） |
| **env 隔离** | `swarmx-pty` 用 `env_clear` 起空环境；只注入白名单：HOME(OAuth)/PATH/LANG/`ANTHROPIC_*`·`OPENAI_*`·`CLAUDE_*` 按前缀放行但受 `blocked_env_prefixes` 拦截 + `SWARMX_AGENT_ID`/`SWARMX_SERVER_URL` + (codex)`CODEX_HOME` | `spawn.rs:242-331` | ✅ |
| **shim / PTY** | `swarmx-shim` exec 前 emit OSC `]633;A`(ready)、exec 后 emit `]633;D;<code>`(exit)；**inherit PTY stdio 让 child `isatty()==true` → 走交互式订阅登录流**（不是降级的非交互模式） | `swarmx-shim/src/main.rs:26-94` | — |
| **pump（单任务）** | drain PTY 输出 → `scan_osc`(生命周期) + `ReadyPlanRunner`(答首启对话框, 如 codex "Hooks need review"→`2\r`) + `HealthScanner`(连续扫 auth/quota needle→诚实失败卡) + recorder(asciicast 录制) + resume buffer | `spawn.rs:398-420` | ✅ ready_plan/health needle 都在 toml |
| **活动/思考轨迹** | **tail CLI 自己写的磁盘 session JSONL**：claude `~/.claude/projects/<编码cwd>/<sid>.jsonl`（sid 由 `--session-id` 强制）、codex `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`。700ms 字节偏移 tail → `parse_{claude,codex}` → `ParsedTool::{Start,End}` → `AgentActivity` → 派生 `ThoughtTrace` → 前端 | `transcript.rs`（`Flavor`枚举 50；locate 222；parse 534/581） | ❌ **两路硬编码 `Flavor`**，label 仅 48 字符、无 diff |
| **唤醒（命脉）** | **(a) Stop hook 可靠兜底**：pre_spawn 写的 hook 每个 turn 边界跑 `swarmx-mcp wake-check` → `POST /api/message/consume_wakes`（原子领取+标已读）→ 有未读 wake 就 emit `{"decision":"block","reason":...}` 强制续跑，reason 引导 agent 调 `swarm_list_blackboard`/`swarm_list_messages`。**(b) PTY kick best-effort**：`deliver_wake`/`deliver_manual_wake` 先写 mailbox(真相源)再注 `\x15<text>`+150ms+`\r`（codex bracketed-paste 必须拆写）。kick 失败 → 下次 Stop 兜住 | `wake.rs`、`wake_check.rs`、`pre_spawn.rs:592-672` | Stop hook 格式枚举驱动；kick 是 PTY 专属 |
| **唤醒决策** | `WakeCoordinator`：`select_targets`/`depends_on`/`.error`·`.failed` fan-out/autokill/orphan/rewake/cycle 检测——**纯依赖逻辑、与传输无关**（决定 wake 谁，不管怎么 wake） | `wake.rs:110-904` | ✅ 传输无关 |
| **计费红线** | `enforce_billing_policy`：claude 锁 `interactive-subscription` + `blocked_env_prefixes=["ANTHROPIC_"]`（防 ambient API key 把它切到 API 计费） | `spawn.rs:447`、`billing.rs` | ✅ |
| **前端** | 绝大多数已由 `/api/plugins`+`/api/models` 数据驱动（spawn 按钮、模型卡 `native_tiers` 门、输入策略 `input_settle_ms`、EmptyState 就绪检测）。**真正要改码**：MCP admin 面板(`type Cli`+`mcp_admin.rs` 写 user-级 MCP)、`rest.rs install_hint_for`、`plugins.rs builtin embed`、**`roles/orchestrator.md`（队长运行时真正"选引擎"处）** | 见前端清单 | 大多 ✅ / 4 处需改 |

**一句话**：claude/codex 之所以能在 PTY 黑盒下协调，全靠 **① 各自有能返回 `block` 的 Stop hook（可靠唤醒兜底）+ ② 各自往磁盘写可字节 tail 的 JSONL（活动来源）**。opencode 这两点都不一样 —— 这决定了方案形状。

---

## 1. OpenCode 关键差异（源码确认，v1.17.7）

| 维度 | claude/codex | opencode | 影响 |
|---|---|---|---|
| **续跑 Stop hook** | 有（settings.local.json / hooks.json，返回 `block`） | **无**。`Hooks.event` 收到 `session.idle` 但返回 `Promise<void>`，无法 block（`plugin/src/index.ts:222-335`）。但 plugin 持 `client`(SDK)+`$`(shell) | 唤醒要么靠 **JS 插件自注入**(A)，要么靠 **ACP swarmx 掌循环**(B) |
| **活动存储** | 单文件 JSONL，可字节 tail | **JSON 文件树**：`storage/session/{info,message,part}/…json`，每条消息/part 一个小 JSON（`storage/storage.ts:64-197`，根 `$XDG_DATA_HOME/opencode`，`core/global.ts:11`） | 不能字节 tail → **活动走事件流（插件 push / SSE / ACP 通知），不 tail 文件** |
| **配置隔离** | claude `--strict-mcp-config` 单文件；codex per-agent `CODEX_HOME` 全局剥离 | **项目本地 `<ws>/opencode.json`** 直接带 `mcp`+`permission`+`autoupdate`+`plugin`（schema `core/v1/config/mcp.ts:6`：`type:"local"`/`command[]`/`environment`/`enabled`/`timeout`） | **比 codex 更干净**：不碰全局、不撞车 |
| **auth** | claude Keychain / codex `auth.json` | `$XDG_DATA_HOME/opencode/auth.json`（`auth/index.ts:10`） | 共享、登录一次，**不需要 per-agent 隔离** |
| **跳过审批** | flag（`--dangerously-skip-permissions` / `--dangerously-bypass-...`） | **无 flag**；`permission` 块全 `allow`（默认已很放权） | 写进 `opencode.json`，非 argv |
| **信任弹窗** | claude 有（pre-patch） | **无**（源码 grep 零命中） | `trust_format=None`，`ready_plan` 大概率空 |
| **ACP** | claude 故意不开（计费红线）；codex `app-server`(元数据未接) | **`opencode acp` 原生**：官方 `@agentclientprotocol/sdk` + `ndJsonStream`，与 swarmx `acp.rs` 的 `LineDecoder` 分帧**完全一致**（`cli/cmd/acp.ts:3,55`；`src/acp/` 全套 agent/session/permission/tool/event） | B 路可行且地基已在 |
| **模型/effort** | `--model {v}` + `--effort/-c`（argv） | `--model provider/model`（run）；TUI/effort 走 `opencode.json`(`model` + `providerOptions.reasoningEffort`) | manifest 的 model 可走 argv；effort 可能要写进 opencode.json（实现期实测确认） |

---

## 2. 方案 A —— PTY + swarmx opencode 插件（先做）

**思路**：让 opencode 成为和 claude/codex **同构的 PTY worker**，复用 PTY/录制/health/OSC/ready_plan/计费/前端全套；只用一个 JS 插件补上"可靠唤醒"这条缺腿。

| 步骤 | 做什么 | 对应现有代码 | 新增/改动 |
|---|---|---|---|
| **A1 manifest + 内嵌** | `cli-plugins/opencode.toml`：`binary="opencode"`、`home_env=HOME`、`model_args=["--model","{model}"]`、`native_tiers=false`、`input_settle_ms`(实测)、`health_needles`(实测未登录串)、`transport="pty"`。加入 `plugins.rs` `BUILTIN` 的 `include_str!` + 改 `builtin_ships_*` 测试 | `plugins.rs:400-420`、`cli-plugins/codex.toml`(模板) | 1 toml + 1 行 embed |
| **A2 新格式 writer** | `plugins.rs` 加枚举变体 `McpFormat::OpencodeJson` / `StopHookFormat::OpencodePlugin`（`TrustFormat::None` 复用）；`pre_spawn.rs:run_patches` 加 match 臂，writer 幂等写 `<ws>/opencode.json`：`mcp.swarmx-swarm`(command=[mcp_bin], `environment`={SWARMX_AGENT_ID,SWARMX_SERVER_URL}) + `permission` 全 allow + `autoupdate:false` + `plugin:["<swarmx 插件路径>"]` | `pre_spawn.rs:673` 派发、`patch_claude_mcp_at`(模板) | 2 枚举值 + 1 writer + 1 match 臂 |
| **A3 唤醒插件** | swarmx 自带 opencode JS 插件（随 sidecar 打包）：`event` 收 `session.idle` → 跑 `swarmx-mcp wake-check`（经 `$` 或直接 `consume_wakes`）→ 返 `block` 则 `client.session.prompt({parts:[{type:"text",text:reason}]})` 续跑。reason 文本复用 `wake_check.rs:226`。防循环靠 server 端 `consume_wakes` 返 0 + 节流文件 | `wake_check.rs`(复用)、`routes/swarm.rs:200 consume_wakes`(复用) | 1 个 TS 插件产物 |
| **A4 活动接入** | 不 tail JSON 树。两选一：**(推荐)** A3 插件顺带把 tool/part 事件 POST 给 swarmx 新端点 → `AgentActivity`；或 `transcript.rs` 加 `Flavor::Opencode` 走目录监视。优先插件 push（一个插件给唤醒+活动两用） | `transcript.rs`(Flavor)、`swarm.rs record_activity`(下游复用) | 1 个 activity 注入端点 |
| **A5 公共触点** | `tools.rs:157` `enum:["claude","codex"]` 加 `opencode`；`rest.rs install_hint_for` 加 opencode 臂；`roles/orchestrator.md` 教队长**何时选 opencode**（否则永不被 spawn）；前端 MCP admin（可选，需 opencode user-级 MCP 写法） | `tools.rs:157`、`rest.rs:246`、`orchestrator.md` | 局部 |
| **A6 PTY kick** | `deliver_wake` 对 opencode 仍可注入 TUI 输入框（live 提醒），可靠兜底是 A3 插件 | `wake.rs:289-367` | 零（复用） |

**验收**：`scripts/golden-cli-test.sh` 起 opencode worker 调 `swarm_write_blackboard` 断言黑板出现 → 隔离后端 live e2e（真 spawn、有未读 mail 能醒并接着干）→ **装包真机零命令可用**（opencode 二进制 + swarmx 插件 + opencode.toml(已 embed) 随包，对齐 CLAUDE.md 头号原则）。
**复用**：PTY/录制/health/OSC/ready_plan/计费/WakeCoordinator/MCP swarm 工具/前端大部分。
**新增**：1 toml + 1 组 format writer + 1 TS 插件 + 1 activity 端点 + orchestrator 提示。

---

## 3. 方案 B —— ACP（已取消，实现已删除）

> **已废止**：ACP 曾有完整实现（`acp.rs` codec + `Connection`、`acp_engine.rs`、
> `Transport` 枚举），2026-06-17 在 commit `b60b2d2` 整体删除。原因：opencode
> 改走 PTY + 官方 `/tui` HTTP 控制接口（方案 A 的落地形态）后，大 prompt 注入
> 问题被 `/tui/*` API 直接解决，ACP 传输失去存在意义。下表仅作历史设计存档，
> 其中引用的 `acp.rs`、`transport=acp` seam、`structured_args` 均已不存在。

**思路**：swarmx 当 ACP client 驱动 `opencode acp`，**自掌 turn 循环** —— 唤醒问题直接消失、拿真 tool/permission/streaming 事件、不 tail 文件、不要插件。

| 步骤 | 做什么 | 对应现有代码 |
|---|---|---|
| **B1 接通 seam** | `spawn.rs` 的 `transport=acp` 不再回退 PTY：piped-stdio 起 `opencode acp --cwd <ws>` → `Connection::spawn`（`acp.rs` 已有）→ `initialize` 握手 | `spawn.rs:117-133`(seam)、`acp.rs:16-26`(Connection 已建测) |
| **B2 session 层** | 建在 `Connection` 上：`session/new` + `session/prompt`；把 ACP 通知(tool_call/permission/streaming/idle) 映射成 `SwarmEvent`(`AgentActivity`/`ThoughtTrace`)；permission 由 ACP client 自动 allow | `acp.rs:24`(标注的待办增量)、`swarm.rs`(下游复用) |
| **B3 唤醒走 ACP** | `deliver_wake`/`deliver_manual_wake` 对 ACP agent 改为发 `session/prompt`(reason 复用)，不再 mailbox+PTY-kick（mailbox 留作台账）。`WakeCoordinator` 决策逻辑**不变** | `wake.rs`(决策复用，注入分支新增) |
| **B4 取舍** | ACP agent 无 PTY → 无终端录制/回放（UI 与 claude/codex 分叉）；health 改判 ACP 错误/断流。`opencode.json` 仍写 mcp+permission（ACP 模式 Server 同样加载 MCP） | — |
| **B5 红利** | 这套 ACP 驱动建成后，**codex app-server / 任何 ACP CLI 都能走** | `codex.toml structured_args`(已备) |

**验收**：opencode 经 ACP 起、收 prompt、tool 事件进 transcript、有 mail 能续跑；live e2e。
**新增**：ACP session 驱动（initialize + 通知→SwarmEvent + piped spawn）+ wake 的 ACP 分支。

---

## 4. 顺序、风险、验收

- **顺序**：先 A（复用最多，最快出一个一等公民 opencode worker），后 B（结构化升级 + 解锁 codex app-server）。A 与 B 可共存：同一 `opencode.toml` 切 `transport` 即可灰度。
- **风险**：① opencode 版本漂移（flag/插件 API，`experimental.*` 钩子不稳）→ 钉版本 + health needle 兜底；② ACP wire schema 需 live `opencode acp` 实测钉死（B2）；③ 计费面 —— 确认 opencode 用对账户，若担心 ambient `*_API_KEY` 把订阅带偏，给 opencode.toml 加 `blocked_env_prefixes`；④ **装包**：opencode 二进制 + swarmx 插件 + `opencode.toml`(已 builtin embed) 都要随包，按头号原则真机验。
- **验收三板斧**：`cargo test --workspace` 绿 → `golden-cli-test` 真起 opencode → 隔离后端 live e2e → 装包真机零命令可用。

---

## 附：A vs B 一图流

```
A（PTY，同构 claude/codex）         B（ACP，swarmx 掌循环）
─────────────────────────         ─────────────────────────
swarmx ──PTY──> [shim] opencode  swarmx ──stdio(ndjson)──> opencode acp
   │  scan OSC/health/record          │  Connection + initialize
   │  写 <ws>/opencode.json            │  写 <ws>/opencode.json(mcp/permission)
   │   (mcp+permission+plugin)         │  session/new → session/prompt
   │                                   │  ACP 通知 → SwarmEvent(activity/trace)
唤醒: JS 插件 session.idle           唤醒: swarmx 看 turn 完成
   → wake-check → client.prompt          → 直接 session/prompt(reason)
活动: 插件 push 事件                 活动: ACP tool/stream 通知
录制/回放: ✅ 复用 PTY               录制/回放: ✗（无 PTY）
缺腿补法: 一个 TS 插件               缺腿补法: 建 ACP session 驱动(还 L4 债)
```
