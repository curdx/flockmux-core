# 全库深审报告(第二轮)— 2026-07-09

**基线:** 第一轮(`docs/deep-review-2026-07-08.md`,11 处修复)之上。
**方式:** 架构师级复审 —— 按跨模块系统性维度分片并行审查(10 片)→ 每条发现派独立 agent 对抗性证伪 → 存活项按「根因聚类」修复(治本,不补窟窿)→ 真实浏览器验证可观测项 → 全绿测试。

原始发现 29 条,证伪/判定已处理 7 条,存活 22 条。本轮把 22 条症状归并为若干**根因簇**,按簇修复。

---

## 一、浏览器实测的价值(先说结论)

用真实 Chrome 驱动隔离栈(`:5188`)后,两条静态「HIGH」被下修、两条新问题被发现:

- **黑板跨文件串写(#3)** —— 逻辑真实存在(700ms 延迟即复现串写),但 `BlackboardPanel` 仅挂在 `VITE_ENABLE_DEBUG` 门控的 `/debug`,**真实用户不可达**;可达的 `Context.tsx` 已用 `cancelled` 守卫。**HIGH → LOW/潜伏**。
- **未读徽标 vs 分隔线(#17)** —— **未复现**,二者共用 `lib/unread.ts`,一致。可达面板无分歧。
- **新发现·深链带 workspace UUID 掉子视图**(已修):`activeWs` 只按 slug 匹配,uuid 形 URL 落不到 → 被当陌生 wsId 重定向丢子路径。
- **新发现·文件浏览器误报**(已修):workspace 目录不存在时,`canonicalize` 失败被归为「越界」,报「path outside workspace」误导用户。

教训:静态审查给的是**候选**;哪些真正咬用户,得靠真实 UI 跑一遍才知道。

---

## 二、已修复(按根因簇)

### 簇 B/C — 打包运行时:子进程 PATH + 家目录统一(项目头号事故源)
- **根因:** 工具性 shell-out 各自 `Command::new`,漏设 `augmented_path()`;家目录解析散落 `env::var("HOME")`,一半懂 Windows 一半不懂。装包后 sidecar 只有 launchd 极简 PATH、可能无 HOME → 功能静默失效。
- **治法:** 两个不可绕过的原语落在 `runtime_path.rs`:`tool_command()/tool_command_async()`(PATH 由构造保证)、`swarmx_home()`(HOME→USERPROFILE 唯一真理)。收编全部非 PTY 工具 shell-out(fusion `zulu`、`zulu list-model`、`--help` 探测、客观闸门 `sh -c`、verify 门、git、`mcp add`、装插件)与全部数据目录/家目录解析。
- **修掉:** #4(装包后融合/研究委员会整块失效,HIGH)、#5(Windows 装包崩溃/状态撕裂,HIGH)、#13(客观闸门被静默架空,MED)、#15(带空格安装路径令 wake hook 失效 —— shell-quote + 回归测试)。
- **验证:** `zulu/models` 200、Consult 14 模型面板正常(浏览器实测无回归)。

### 簇 A — PtyBridge kill 一律离线执行(不阻塞 async worker / 不持锁)
- **根因:** `kill()` 是同步 ~1s SIGTERM 宽限循环;三处 async 调用点(自动解散、`teardown_agent`、终端 reaper)在**持锁**且**在 tokio worker 上**内联跑它 → 一轮 fan-out 同时解散 N 个 worker 会冻结整个服务 ~1s。
- **治法:** `registry::offload_kill`(锁内 clone 出 bridge Arc → 释放锁 → `spawn_blocking(kill)`);终端 reaper 改为「锁内摘出陈旧会话 → 锁外离线 drop」。sync `kill()` 只留给 `Drop`。
- **修掉:** #2、#10(#22 归入 reaper 簇,后续处理)。

### 簇 D — 融合批次生命周期:可持久化恢复 + 进入裁判幂等
- **根因:** 批次推进只靠内存态 tokio 任务(autochain/watchdog),进程重启即丢;进入裁判阶段无 CAS。
- **治法:**
  - #11:`enter_judge_stage` 起点廉价预检 + 翻转点用 `transition_fusion_status('running'→'judging')` 原子抢占;`set_fusion_judge` 只写 judge 线程 id,不再兼职翻状态 → 双击/autopilot 抢跑不再派重复裁判,已 done 批次不会被打回 judging 永久卡死。
  - #18:autopilot 是否推进改为看「有无待裁选手」,而非「是否有 agent spawn 成功」—— 全部 spawn 失败(仍留 user-driven 方向)不再静默停在 running。
  - #9:启动 orphan-sweep 后新增融合恢复扫描 —— 所有仍处 running/judging 的存活批次 → `needs_decision`(UI 手动兜底入口)。诚实、不烧额度(不偷偷重跑 LLM)。
- **验证:** 注入 running/judging 批次 → 重启 → 日志 `fusion recovery: stalled batches → needs_decision recovered=2`,DB 两条均翻为 needs_decision。

### UI —— 深链 / 文件浏览器
- 深链:`activeWs` 与 stale 重定向按 `id`(slug)**或** `workspaceId`(uuid)匹配 —— 一个 workspace 两个标识,解析都认。浏览器实测 uuid 形 `/chat/{uuid}/ledger` 正常渲染子视图、真实 slug 无回归。
- 文件浏览器:区分「目录不存在」与「越界」—— 前者返回 404「workspace directory no longer exists on disk: <cwd>」。浏览器实测文案清晰。

---

## 三、门禁

| 门禁 | 结果 |
|---|---|
| `cargo build --workspace` | ✅ |
| `cargo test --workspace` | ✅ 417 passed / 0 failed |
| `node scripts/harness-check.mjs` | ✅ |
| `web` `npm run build`(tsc) | ✅ |
| 浏览器实测(文件浏览器 / 深链 / Consult) | ✅ 三项 PASS |
| 运行时实测(融合重启恢复) | ✅ recovered=2 → needs_decision |

---

## 四、尚未处理 / closure(2026-08 优化审计核对)

> 核对基线: `reaper.rs` / `wake.rs::spawn_deliver_wake` / reasonix+zulu MCP 写盘。

- **簇 H (reaper 自退出泄漏 #8):** **已关单。** `reaper::spawn` 在 `main` 启动; `sweep_once` 在 grace 后 `registry.remove` 回收 writer 线程 + master fd + live-agent 名额; 单测 `sweep_evicts_exited_slot_after_grace` 覆盖。进程账本见迁移 `0029_agent_shim_pid.sql`。
- **簇 A′ (Wake 内联 await 全域阻塞 #1):** **已关单(缓解到位)。** `spawn_deliver_wake` 把 mailbox 写 + kick 放到独立 task; per-agent `kick_locks.try_lock` 单飞; `delivery_sem`(`MAX_CONCURRENT_KICKS`) 限并发。wedged 引擎只占自己的 lock + 一个 permit,不再阻塞协调器消费循环。
- **簇 F reasonix/zulu 共享 MCP 身份串号 (#16):** **已关单。** 对齐 kimi: workspace 级 MCP 文件**不再**写入 `--agent-id` / `SWARMX_*` env;身份靠 spawn 注入的进程环境继承(`SWARMX_AGENT_ID` / `SWARMX_SERVER_URL`)。Shared 布局下 last-writer 文件内容相同,不再串号。
- **仍开(零散):** reasonix 盲提交双回合(#6);并行同角色 worker 撞 handoff key(#7);codex 污染全局 `~/.codex/config.toml`(#12);spawn_worker 超时+幂等(#14);only_undelivered(#20);thought-trace 事务(#19);handoff 新鲜度门读 op-log(#21);reaper 对被杀 agent 合成假 Error(#22)。
