# 多 CLI 可扩展性重构施工方案

> 目标：把"加一个新 CLI（gemini / copilot / …）"从**改 Rust 代码 ~3 处** 降到**填一张 `cli-plugins/<id>.toml`**。
> 设计综合 openclaw（`CliBackendConfig`+`CliBackendPlugin` 两层）、gstack（`HostConfig`+`validateAllConfigs`）、superpowers（一份内容+薄适配器+golden 测试）、golutra（`post_ready_plan` DSL）、hermes（registry+ABC、ACP transport、分层 override）。
> 配套审查见 `docs/implementation-review-2026-05.md`（F7/F8/F9 即本方案要消灭的问题）。

> **实现进度**（分支 `fix/p0-security-lifecycle`，全测试绿）：
> - ✅ **L2 派发核心已落地**：新增 `TrustFormat`/`McpFormat`/`StopHookFormat` 枚举 + 字段（`plugins.rs`）；`pre_spawn.rs` 的 `run_patches` 从 `match plugin.id` 改为**按格式枚举派发**，删除 `run_claude_patches`/`run_codex_patches`；`spawn.rs` 两处 `if plugin.id==` 改为按 `mcp_format`/`stop_hook_format` 判断；`claude.toml`/`codex.toml` 回填格式字段；加回归守卫测试（shipped manifest 格式断言 + typo 必失败 + 默认 None）。
> - ✅ **F9 已修**：`load_dir` 改为 warn-skip（单个坏 TOML 不再拖垮全部 CLI / 缺目录非致命 / 重复 id 告警）。
> - ✅ **`ready_plan` DSL（answer_dialog）已落地**：新增 `ReadyStep`/`ReadyStepKind`（plugins.rs）+ `ReadyPlanRunner`（spawn.rs）取代写死的 `DialogAutoAnswer`；删除 `auto_answer_hooks_dialog` 布尔；codex 的 "Hooks need review"→`2\r` 改为 `codex.toml` 里的 `[[ready_plan]]` 数据；支持一个 CLI 列多个对话框、零 Rust。8 个 runner 单测 + manifest 守卫断言。启动自检干净。
> - ✅ **F22 已修（bootstrap 去重）**：`spawn_worker` 与 `run_spell` 两份近乎逐字重复的 bootstrap 注入抽成单个 `spawn_bootstrap_inject(registry, rx, agent_id, prompt, BootstrapCtx)`（rest.rs，净 −37 行）；`2500ms` / `150ms` 等时序常量现在各只出现一次 = 唯一改动点。差异（worker raw prompt vs spell render_prompt、日志字段、占位符诊断）经 `BootstrapCtx` 参数化。全测试绿。
> - ✅ **F8 已修 + stop_hook 超时外置**：删除死字段 `mcp_inject`/`ready_detect`（零引用，纯误导）；Stop-hook 超时从两个 Rust 常量（claude 10000ms / codex 10s）搬进 manifest 的 `stop_hook_timeout`（各自原生单位写入），消除"未来 CLI 复用错常量→1000× 超时"的隐患。manifest 守卫断言锁定。
> - ✅ **前端输入策略数据化（L1 收尾）**：`input_settle_ms` 进 manifest → `CliPluginInfo` → `/api/plugins`；前端启动时 `primeInputPolicies` 填缓存，`inputPolicyFor` 据 cli 查表，删掉 `startsWith('codex-')` 硬分支。**颜色**按调研结论（VS Code "contributes" 主题模型：展示/主题色应在客户端按语义 token 解析、不写死 hex 从后端下发）**留在前端**——`GraphPanel.cliColor` 改成查表+默认兜底（去硬 `===` 分支），全量主题化属 legacy GraphPanel 清理。依据：行为(input timing)→后端 manifest；展示(color)→前端主题。
> - ✅ **bootstrap 固定 2500ms → MCP-ready 信号（readiness-probe）**：原计划"等 PTY 的 MCP-ready 横幅"被**实测否掉**——抓了 claude 2.1.158 + codex 0.134 启动全文，两边都**没有稳定的 MCP 横幅**可匹配（比 codex needle 更脆）。改用规范级信号：swarmx-mcp 在收到 CLI 的 `tools/list` 时 POST `/api/agent/:id/mcp-ready`（MCP 生命周期里 = 工具已暴露给模型）→ server 翻转 per-agent `watch` 门 → bootstrap 等它（changed 或 6s 兜底，不持锁过 await）。实测 MCP **~283ms 就绪**（旧固定 2500ms，约快 9×），bootstrap 走 mcp-ready 路径、orchestrator 拿到工具正常发言、无报错。依据：[MCP lifecycle spec](https://modelcontextprotocol.io/specification/2025-03-26/basic/lifecycle) + [K8s readiness-probe](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/)（固定 sleep 是反模式）。
> - ✅ **L3 golden 验收测试**：`scripts/golden-cli-test.sh` —— 每个 cli-plugin 起一个 worker、唯一任务是调 `swarm_write_blackboard`，轮询黑板断言 key 出现，从而验证"spawn→pre-spawn patch→bootstrap 到达模型→MCP swarm 工具→server 往返"整链路。仿 superpowers `run-test.sh`；非 cargo/CI 测试（真起 CLI、需登录、花少量 token），未装 CLI 自动 skip，内置 `SWARMX_SERVER_URL==PORT`。实测 claude + codex 均 PASS。把"加 CLI"变成可手动跑的合约。
> - ✅ **L5 spawn 上限（修 fork-bomb F4）**：spawn 公共路径 `spawn_with_bookkeeping` 加**全局存活 agent 上限**（数 in-memory registry = 真并发上限，覆盖 /api/agent + /api/worker + run_spell）；`spawn_worker` 加**委派深度上限**（走 workers 表 parent 链，loop-bounded，终止于非-worker 的 orchestrator）。均 env 可调（默认 `SWARMX_MAX_LIVE_AGENTS=256` / `SWARMX_MAX_SPAWN_DEPTH=6`），超限返回 429 + 可操作提示。实测 cap=2 时第 3 个 spawn 被 429 拒。依据 hermes MAX_DEPTH + openclaw 并发 lanes。
> - ✅ **L5a 分层 registry override**：`PluginRegistry::load_layered(dirs)` 按序加载 bundled `cli-plugins/` + 用户 `~/.swarmx/cli-plugins/`（env `SWARMX_USER_CLI_PLUGINS_DIR` 可改），同 id 后层覆盖前层（last-writer-wins）+ 覆盖时 info 点名。per-layer warn-skip 韧性保留；缺层/全缺非致命。用户不 fork 仓库即可改/加 CLI。单测（覆盖赢、用户新增、坏 TOML 跳过、缺层 no-op）+ boot 冒烟（REST `/api/plugins` 实测 codex 被用户层覆盖、gemini 新增）。
> - ✅ **L5c model 与 CLI 解耦（gstack "host ≠ model"）**：manifest 加 `model_args` 模板（`{model}` 占位符，claude/codex 均 `["--model","{model}"]`）+ 可选 `default_model`；spawn 时按 `req.model || plugin.default_model` 解析并替换进 argv（纯函数 `model_overlay_args`，单测）。`SpawnAgentRequest`/`SpawnWorkerRequest` + MCP `swarm_spawn_worker` schema 加 `model` 参数，orchestrator 可给不同 worker 指定不同模型而不 fork CLI id/role；`CliPluginInfo`/前端 types 下发 `default_model`。同 CLI 任意模型、零 Rust/role 分叉。端到端冒烟（echo 假 CLI 经 REST 带 model spawn，录制实测 argv 出现 `--model opus-smoke`）。未做 DB 持久化 model（归前端那轮一起做 UI badge）。
> - ✅ **L4 ACP 传输基础**：①manifest 加 `transport = "pty" | "acp"` 枚举（默认 pty，claude/codex 零变化）；②新增 `crate::acp` —— 把 JSON-RPC-over-stdio **抽成单一可复用 codec**（hermes 反面教材是重复 3 份）：Request/Notification/Response/Error 类型 + 行分帧 `LineDecoder`（缓冲半包、跳空行、容忍 CRLF、坏帧不毒化流）+ 单调 `IdGen`，7 个纯函数单测；③spawn.rs 接缝——声明 `acp` 时 warn 并回退 PTY（codec 就绪、session 驱动是下一步增量）。端到端冒烟：echocli 声明 acp → 日志正确 warn、echo 仍经 PTY 回退正常 spawn（声明 acp 安全不破坏）。**待续（远期）**：ACP session 驱动（initialize 握手 + permission/tool-call 事件映射 + streaming），建在此 codec 上。
> - ✅ **ready_plan 顺序 onboarding**：ReadyPlanRunner 从"并行 answer_dialog 监视器"重构为**顺序游标状态机**，支持 4 种步骤——`answer_dialog`（等 needle→注入 response→前进）、`wait_for`（等 needle 出现再前进，取代固定 sleep）、`input`（步骤激活即注入）、`extract_session_id`（抓 needle 后 token 进 `into` 槽，备 resume）。每步单发、needle 步超时（window_ms 默认 30s 到点跳过）、buffer 8KiB 滑窗。codex 单 answer_dialog 行为不变。9 个 runner 单测。暂无 shipped CLI 用新步骤（备未来 onboarding）。
> - ✅ **L4 ACP `Connection` 层**：在 codec 之上加了异步 `Connection`（`crate::acp`）——把 JSON-RPC-over-stdio 抽成单一组件（hermes 反面教材重复 3 份）：按 id 关联 request↔response、notification / 对端 request 走 channel、写串行化、reader task EOF 时让 pending 的 request 拿到 `Closed` 不挂死。tokio duplex 模拟对端的 4 个单测（往返 / RPC error / notification+对端 request 投递 / 对端关闭）。
> - ❌ **L4 ACP 已整体删除（2026-06-17，commit `b60b2d2`）**：上面两条（codec/`Connection`）连同 `acp_engine.rs`、`Transport` 枚举、`structured_args` 一起移除。原因：opencode 改走 PTY + 官方 `/tui` HTTP 控制接口后不再需要 ACP 绕过 TUI 注入问题，ACP 传输失去存在意义。原"待做"的 ACP session 层随之取消。
> - 现状净效果：**第三个 CLI 若复用 claude/codex 的配置格式 = 纯填 `cli-plugins/<id>.toml`，零 Rust**；若格式全新 = 加 1 个枚举值 + 1 个 writer + 1 个 match 臂（局部，不再散落）。

---

## 0. 现状（为什么要改）

`cli-plugins/<id>.toml` 现在只是**功能开关表**，真正的 per-CLI 行为硬编码在 Rust，按字面 `plugin.id` 分散在 4 处：

| 关注点 | 现状（硬编码位置） | 问题 |
|---|---|---|
| 写 MCP 配置 | `pre_spawn.rs:544 match id` → claude=`~/.claude.json` local + per-agent file；codex=`~/.codex/config.toml` global | 通道+格式焊死在 id 分支 |
| 写 trust | 同上 match id | 同上 |
| Stop hook | `pre_spawn.rs` 两个常量：claude `timeout=10000`(ms)、codex `timeout=10`(s) | 超时单位是"目标 CLI 的属性"，却当 Rust 常量；用错 1000× |
| argv 注入 | `spawn.rs:106 if id=="codex"` / `spawn.rs:122 if id=="claude"` | inline if，新 CLI 易漏 |
| 拉起对话框 | `spawn.rs:399 DialogAutoAnswer` 匹配英文 `"Hooks need review"`→`2\r` | 绑死 codex 0.132，换语言/菜单序就答错 |
| 死字段 | `plugins.rs:19-23 mcp_inject / ready_detect` | 解析但**零功能读取**，误导（F8） |
| registry 韧性 | `plugins.rs:102 load_dir` 用 `?` | 一个坏 TOML 拖垮全部 CLI（F9） |
| 前端 | `cliInputPolicy.ts startsWith('codex-')`、`GraphPanel.cliColor` | per-CLI 知识硬编码在前端 |

加 `gemini.toml` 后：能 spawn，但 `run_patches` 命中 `other => noop` → **无 MCP（没有 swarm 工具，无法协调）/无 trust（headless 卡在信任弹窗）/无 wake**。等于又聋又哑（F7）。

**核心思路**：派发改为 keyed on **能力枚举**，而非 **id 字符串**。CLI 若复用已知配置格式 = 纯填表；若格式全新 = 加一个 writer + 一个枚举值。

---

## 1. L1 — manifest 升级为"行为描述符"

把 per-CLI 知识从 Rust 常量/分支搬进 TOML。`cli-plugins/gemini.toml`（示意）：

```toml
id           = "gemini"
display_name = "Gemini CLI"
binary       = "gemini"
home_env     = "HOME"
# 跳过审批的旗标（claude=--dangerously-skip-permissions / codex=--dangerously-bypass-... / gemini=--yolo）
skip_approvals_args = ["--yolo"]

[mcp]                       # 取代死字段 mcp_inject + match id
mode = "gemini-settings-json"   # 枚举: claude-config-file | codex-config-toml | gemini-settings-json | none

[trust]
mode = "settings-json"          # 枚举: claude-json | codex-toml | settings-json | none

[stop_hook]                 # 把 claude(ms)/codex(s) 的单位分歧外置成数据
enabled      = true
file         = ".gemini/hooks.json"
format       = "json"           # json | toml
timeout_unit = "ms"             # ms | s
timeout      = 10000

[input]                     # 取代前端 cliInputPolicy.ts 的 startsWith 硬编码
settle_ms       = 300
bracketed_paste = true

[ui]                        # 取代 GraphPanel.cliColor 硬编码
node_color = "#16a34a"

# golutra 的 post_ready_plan DSL —— 取代 DialogAutoAnswer + 魔法 2500ms。
# 拉起后按抓取到的 PTY 快照顺序执行，每步可带 timeout。
[[ready_plan]]
kind = "answer_dialog"      # 见到 needle 就注入 response
needle = "Trust this folder"
response = "1\r"
[[ready_plan]]
kind = "wait_for"           # 等到某 pattern 出现再继续（取代固定 sleep）
pattern = "(MCP|servers?)\\s+ready"
timeout_ms = 8000
```

对应 `plugins.rs` 结构骨架（新增类型用 `#[serde(default)]`，保证现有 claude.toml/codex.toml 不改也能解析）：

```rust
// crates/swarmx-server/src/plugins.rs
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliPlugin {
    pub id: String,
    pub display_name: String,
    pub binary: String,
    #[serde(default)] pub default_args: Vec<String>,
    #[serde(default)] pub skip_approvals_args: Vec<String>,
    #[serde(default = "default_home_env")] pub home_env: String,
    #[serde(default)] pub mcp: McpSpec,
    #[serde(default)] pub trust: TrustSpec,
    #[serde(default)] pub stop_hook: StopHookSpec,
    #[serde(default)] pub input: InputSpec,
    #[serde(default)] pub ui: UiSpec,
    #[serde(default)] pub ready_plan: Vec<ReadyStep>,
    // ❌ 删除：mcp_inject / ready_detect（死字段，F8）
    // 旧 auto_* 布尔在迁移期保留，迁移完成后删（见 §4）
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum McpMode { #[default] None, ClaudeConfigFile, CodexConfigToml, GeminiSettingsJson }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpSpec { #[serde(default)] pub mode: McpMode }

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TimeUnit { #[default] Ms, S }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct StopHookSpec {
    #[serde(default)] pub enabled: bool,
    pub file: Option<String>,
    #[serde(default)] pub format: HookFormat,      // Json | Toml
    #[serde(default)] pub timeout_unit: TimeUnit,  // ← 单位变成数据，杜绝 1000× 错误
    #[serde(default)] pub timeout: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadyStep {
    AnswerDialog { needle: String, response: String, #[serde(default)] timeout_ms: Option<u64> },
    WaitFor { pattern: String, timeout_ms: u64 },
    Input { text: String },
    // 未来: ExtractSessionId { after: String, into: String } （golutra resume 支持）
}
// McpSpec/TrustSpec/InputSpec/UiSpec 同理，都带 Default。
```

---

## 2. L2 — `CliAdapter` trait + 按枚举派发 + `validate_all`

把 `pre_spawn.rs:544 match plugin.id` 换成"按能力枚举选 writer"。每个 writer 是"某种配置格式"，与具体 CLI 解耦——**新 CLI 复用已知格式就零新代码**。

```rust
// crates/swarmx-server/src/pre_spawn.rs
pub struct PreSpawnCtx<'a> {
    pub agent_id: &'a str,
    pub mcp_bin: &'a Path,
    pub server_url: &'a str,
}

/// 写某种格式的 MCP 配置。按 plugin.mcp.mode 选实现，而非 plugin.id。
fn write_mcp(plugin: &CliPlugin, ws: &Path, ctx: &PreSpawnCtx) -> Result<()> {
    match plugin.mcp.mode {
        McpMode::None => Ok(()),
        McpMode::ClaudeConfigFile  => write_mcp_claude(plugin, ws, ctx),   // 复用现有逻辑
        McpMode::CodexConfigToml   => write_mcp_codex(plugin, ws, ctx),    // 复用现有逻辑
        McpMode::GeminiSettingsJson => write_mcp_gemini_settings(plugin, ws, ctx), // 新增 1 个
    }
}
// write_trust / write_stop_hook 同样按 plugin.trust.mode / plugin.stop_hook 派发。
// Stop hook 的 timeout 由 plugin.stop_hook.{timeout,timeout_unit} 提供，不再是常量。

/// 取代 run_patches 的 match id。任何 CLI 走同一条路径，差异全在数据。
pub fn run_patches(plugin: &CliPlugin, ws: &Path, ctx: &PreSpawnCtx) -> Result<()> {
    if plugin.trust.mode != TrustMode::None { write_trust(plugin, ws)?; }
    if plugin.mcp.mode  != McpMode::None    { write_mcp(plugin, ws, ctx)?; }
    if plugin.stop_hook.enabled             { write_stop_hook(plugin, ws, ctx)?; }
    Ok(())
}
```

`spawn.rs` 的 inline `if id==` argv 注入 → 由 manifest 数据驱动：

```rust
// crates/swarmx-server/src/spawn.rs —— 取代 spawn.rs:106/122 的两个 if id==
let mut argv = vec![shim, plugin.binary.clone()];
argv.extend(plugin.default_args.iter().cloned());
argv.extend(plugin.skip_approvals_args.iter().cloned());
// claude 的 --mcp-config 这类"按 MCP 模式决定的 argv"：
argv.extend(mcp_extra_argv(plugin, ctx));   // ClaudeConfigFile => ["--mcp-config", file, "--strict-mcp-config"]
```

`ready_plan` 引擎取代 `DialogAutoAnswer` + 魔法 2500ms（在 PTY pump 任务里跑，按抓到的快照推进）：

```rust
// 伪代码：在 spawn 的 pump 任务里，依 plugin.ready_plan 顺序推进
for step in &plugin.ready_plan {
    match step {
        ReadyStep::WaitFor { pattern, timeout_ms } => wait_for_pattern(&mut snapshots, pattern, *timeout_ms).await?,
        ReadyStep::AnswerDialog { needle, response, .. } => if snapshot_contains(needle) { input_tx.send(response).await?; },
        ReadyStep::Input { text } => input_tx.send(text).await?,
    }
}
```

`validate_all` + warn-skip 加载（修 F9 + gstack 校验）：

```rust
// crates/swarmx-server/src/plugins.rs
impl PluginRegistry {
    pub fn load_dir(dir: &Path) -> Result<Self> {
        if !dir.is_dir() { return Ok(Self::default()); }       // 缺目录不致命（对齐 roles.rs）
        let mut plugins = HashMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
            match Self::parse_one(&path) {
                Ok(p) => { plugins.insert(p.id.clone(), p); }
                Err(e) => tracing::warn!(?path, error=%e, "skip malformed cli-plugin"), // ← warn+skip，不再拖垮全部（F9）
            }
        }
        let reg = Self { plugins };
        reg.validate_all()?;   // 见下
        Ok(reg)
    }

    /// gstack validateAllConfigs：声明了某 mode 却没有对应 writer / 重复 id / stop_hook.enabled 但缺 file → 启动期报错，
    /// 而不是 spawn 时静默降级成"协调死岛"。
    fn validate_all(&self) -> Result<()> {
        for p in self.plugins.values() {
            if p.stop_hook.enabled && p.stop_hook.file.is_none() {
                anyhow::bail!("plugin '{}': stop_hook.enabled but no file", p.id);
            }
            // mcp.mode / trust.mode 是穷尽枚举，编译期即保证有 writer；
            // 这里再校验 ready_plan 正则可编译、id 唯一等。
        }
        Ok(())
    }
}
```

`tools.rs:149` 的 spawn_worker enum `["claude","codex"]` → 动态由 registry 生成（或直接删枚举，靠服务端 `plugins.get()→NOT_FOUND` 兜底，附一句"available: claude, codex, gemini"）。

---

## 3. L3 — 每 CLI golden 验收测试（superpowers run-test.sh）

把"加 CLI"变成**可测合约**，专杀"装了但 bootstrap 没接上模型"这类最常见回归：

```rust
// crates/swarmx-server/tests/cli_golden.rs（每个 plugin 一条，CLI 不在 PATH 则 skip）
#[tokio::test]
async fn gemini_bootstrap_reaches_model_and_fires_a_swarm_tool() {
    if which::which("gemini").is_err() { eprintln!("skip: gemini not installed"); return; }
    let h = spawn_test_agent("gemini", BOOTSTRAP_PROMPT_THAT_ASKS_FOR_swarm_list_blackboard).await;
    // 断言：① ready_plan 跑完（trust 弹窗被答、MCP ready）；② 输出/事件流里出现 swarm_list_blackboard 调用痕迹
    assert!(h.event_stream_contains("swarm_list_blackboard").await, "swarm tool never fired — bootstrap didn't reach the model");
    h.kill().await;
}
```

---

## 4. 增量迁移步骤（每步可独立验证、不破坏现状）

1. **加结构不改行为**：把 §1 的 `[mcp]/[trust]/[stop_hook]/[input]/[ui]/ready_plan` 加进 `CliPlugin`，全 `#[serde(default)]`；旧 `auto_*` 字段暂留。此时 `cargo build && cargo test` 必须仍全绿。
2. **回填两份现有 manifest**：给 `claude.toml`/`codex.toml` 写上与当前硬编码**完全等价**的 `[mcp]/[trust]/[stop_hook]` 值（claude: `mcp.mode=claude-config-file`, `stop_hook.timeout=10000,unit=ms`；codex: `codex-config-toml`, `timeout=10,unit=s`, 加 `ready_plan` 的 "Hooks need review"→`2\r`）。
3. **改派发**：`run_patches` 从 `match id` 改成按枚举派发（§2），writer 函数体复用现有 `run_claude_patches/run_codex_patches` 的内部逻辑。**用现有 96 个 server 测试 + pre_spawn 测试验证行为零变化**。
4. **搬 argv**：`spawn.rs` 两个 `if id==` 移到数据驱动（`skip_approvals_args` + `mcp_extra_argv`）。
5. **搬拉起逻辑**：`DialogAutoAnswer` + 2500ms → `ready_plan` 引擎；先让 codex 的 ready_plan 复刻当前行为。
6. **删死字段**：移除 `mcp_inject`/`ready_detect`（F8）及迁移完的旧 `auto_*`。
7. **前端数据化**：`/api/plugins` 的 `CliPluginInfo` 带上 `input.settle_ms` / `ui.node_color`；`cliInputPolicy.ts`、`GraphPanel` 改读它，删 `startsWith` / 颜色硬编码。
8. **韧性**：`load_dir` 改 warn-skip + `validate_all`（F9）。
9. **golden 测试**：补 §3。

每步都跑 `cargo test --workspace`（现在 CI 会 gate，见 `.github/workflows/ci.yml`）。

---

## 5. 验收：加 gemini 应该是什么体验

**情况 A（gemini 用类 settings.json 格式，且已实现该 writer）= 纯填表**：丢一个 `cli-plugins/gemini.toml`（§1 那份），重启 server。gemini 立即获得 trust patch、MCP swarm 工具、wake hook、节点颜色、输入策略——能正常加入 swarm 协作。**零 Rust 改动。**

**情况 B（gemini 配置格式全新）= 填表 + 1 个 writer + 1 个枚举值**：
- `plugins.rs`：`McpMode` 加 `GeminiSettingsJson` 变体；
- `pre_spawn.rs`：实现 `write_mcp_gemini_settings`（约 30 行，照 claude/codex writer 抄）；
- 其余（trust/hook/argv/dialog/颜色/输入）全在 `gemini.toml` 里。
对比现状（§0 的 4+ 处分散硬编码 + 易漏），这是从"改散落代码"到"加 1 个内聚 writer + 填表"。

---

## 6. L4 / L5（远期，本方案先不做，留接口）

- **L4 结构化协议传输**（hermes ACP/app-server）：manifest 加 `transport = "pty" | "acp" | "app-server"`。对 Codex `app-server` / Copilot `--acp`，走 JSON-RPC 拿真 tool-call/permission/streaming 事件，取代刮 PTY + 答英文对话框。PTY 留作通用兜底。**把 JSON-RPC-over-stdio 抽成一个组件**（hermes 反面教材：它重复实现了 3 份）。`ready_plan`/`DialogAutoAnswer` 在 ACP 模式下不需要。
- **L5a 分层 registry override**（hermes）：扫 bundled `cli-plugins/` + 用户 `~/.swarmx/cli-plugins/`，按 id last-writer-wins，用户不 fork 仓库即可改/加 CLI。
- **L5b spawn 上限 + 能力交集**（hermes/openclaw lanes）：spawn 深度上限、fan-out 上限、并发 lane——修 fork-bomb（审查报告 F4），加固 skip-permissions 姿态。
- **L5c model 与 CLI 解耦**（gstack "host ≠ model"）：加可选 model overlay 轴，behavior 微调不分叉 role。

---

## 7. 风险与回滚

- **行为漂移**：步骤 2-3 的等价回填是关键风险点。缓解：回填后用现有 `pre_spawn` 单测 + 96 个 server 测试做"黄金对照"，必要时加一个"渲染出的 MCP/hook 文件内容快照测试"锁定字节级等价。
- **`ready_plan` 取代 DialogAutoAnswer**：codex 拉起时序敏感。缓解：步骤 5 先让 codex ready_plan 一比一复刻，灰度验证后再清理旧 `DialogAutoAnswer`。
- **回滚**：每步是独立 commit；L1 加字段、L2 改派发、前端数据化彼此解耦，任何一步出问题可单独 revert，不影响已 ship 的 P0 修复。
- **不在本方案内**：协调正确性（wake/blackboard 的 F3/F6/F12/F13）是另一条线，见审查报告 P1，互不依赖。
