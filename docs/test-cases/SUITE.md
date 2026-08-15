# swarmx 全量测试用例

格式：`TC-<域><序号>` · **P0/P1/P2** · 可选 `💰`（烧额度）· 可选 `📦`（装包）

勾选栏：`[ ]` 未跑 · `[x]` PASS · `[!]` FAIL · `[-]` SKIP

---

## A. 冷启动 / First-run

### TC-A01 · P0 · 首页可达
- **前置：** 栈或装包已起
- **步骤：** 打开前端根 URL
- **期望：** 落到 `/chat` 或欢迎/空态；无白屏；无「假装还没建空间」的后端挂死态
- **结果：** [ ]

### TC-A02 · P0 · 后端不可达 vs 真的没空间
- **步骤：** 停后端；刷新前端
- **期望：** 明确「连不上后端」类错误（Home / banner），**不是**空工作空间欢迎文案
- **结果：** [ ]

### TC-A03 · P0 · Welcome → 新建向导
- **前置：** 无 workspace（或清数据）
- **步骤：** 走欢迎 CTA → CreateWizard 打开
- **期望：** Dialog 可填名字 + 选目录；点 backdrop **不会**丢掉已填内容
- **结果：** [ ]

### TC-A04 · P0 · 💰 新建空间并拉起队长
- **步骤：** 填名、选真实项目目录、提交；等扫描/进群
- **期望：** workspace 出现；队长上线（或明确失败卡）；聊天可发消息
- **结果：** [ ]

### TC-A05 · P1 · 双击防重入
- **步骤：** 向导提交时连点两次「创建」
- **期望：** 只创建一个 workspace / 一个 orchestrator（不双拉）
- **结果：** [ ]

### TC-A06 · P1 · 扫描超时 / 跳过仍进群
- **步骤：** 提交后点跳过或等到超时
- **期望：** 仍进入聊天；队长可在后台继续；不卡死在 loading
- **结果：** [ ]

### TC-A07 · P0 · 引擎全未就绪空态
- **前置：** 无可用 CLI（或 mock）
- **期望：** EmptyState 说清缺什么；给装引擎入口；不假装能干活
- **结果：** [ ]

### TC-A08 · P0 · 部分引擎可用
- **前置：** 仅 claude 可用
- **期望：** 明示可用引擎；允许继续；缺失引擎可「稍后补齐」
- **结果：** [ ]

### TC-A09 · P1 · 已装未登录 / 不可用
- **期望：** `installed` ≠ `usable`；needs_login 有指引；绿标有证据
- **结果：** [ ]

### TC-A10 · P1 · 📦 init spell 装包后存在
- **步骤：** 装包打开 → `/api/spells` 或新建空间
- **期望：** 有 `init`；无「后端未加载 init spell」
- **结果：** [ ]

### TC-A11 · P1 · 📦 BackendDownBanner（sidecar）
- **步骤：** 装包态杀 sidecar / 等挂
- **期望：** banner 重试 / 失败 / 可手动重启；不是静默空白
- **结果：** [ ]

### TC-A12 · P2 · 语言与主题默认
- **期望：** 默认中文可用；主题 light/system 可切换且持久
- **结果：** [ ]

---

## B. 工作空间与方向

### TC-B01 · P0 · 侧栏列表与切换
- **步骤：** ≥2 个 workspace 间切换
- **期望：** URL/消息/成员随空间变；无串台
- **结果：** [ ]

### TC-B02 · P0 · 删除工作空间
- **步骤：** 删当前空间（确认）
- **期望：** 活 agent 被杀；列表移除；落到合理页
- **结果：** [ ]

### TC-B03 · P1 · 新建方向（未命名）
- **步骤：** 「新方向」→ 说话 → 期望命名
- **期望：** 起队长；可 `swarm_name_thread`；后台可隔离
- **结果：** [ ]

### TC-B04 · P1 · 命名方向 + worktree
- **前置：** cwd 是 git 仓库
- **步骤：** 建命名方向
- **期望：** preparing → ready；独立 worktree；失败则 degraded + 可恢复提示
- **结果：** [ ]

### TC-B05 · P1 · 非 git 目录
- **期望：** 隔离不可用时有清晰禁用/提示，不假成功
- **结果：** [ ]

### TC-B06 · P1 · 切方向隔离
- **步骤：** 两方向各有消息/成员
- **期望：** URL `/t/:slug`；内容不串；ledger key 前缀正确
- **结果：** [ ]

### TC-B07 · P1 · 删方向
- **步骤：** 删非 main 方向
- **期望：** 确认 → kill → 删卡；若正在该方向则回 main
- **结果：** [ ]

### TC-B08 · P1 · 合并到主线
- **前置：** isolated+ready 非 main
- **步骤：** 「合并到主线」走完
- **期望：** 合并成功或冲突可读；可清理 worktree/卡
- **结果：** [ ]

### TC-B09 · P2 · 移动端侧栏
- **期望：** Sheet 可开合；不挡主内容操作
- **结果：** [ ]

### TC-B10 · P2 · ahead/behind / dirty 展示
- **期望：** git 状态可读，不吓人误报
- **结果：** [ ]

---

## C. 聊天核心

### TC-C01 · P0 · 💰 发消息拉起/唤醒队长
- **步骤：** 空房间发一条明确短任务
- **期望：** 队长上线；消息进列表；有回复或明确进行中
- **结果：** [ ]

### TC-C02 · P0 · 💰 队长用 swarm_send_message 回用户
- **步骤：** 要求 `kind=reply` 回聊天
- **期望：** 用户气泡侧看到 reply，不只终态在终端里
- **结果：** [ ]

### TC-C03 · P0 · Composer 发送与草稿
- **步骤：** 输入 → 切方向 → 回来；再发送
- **期望：** 草稿按方向恢复；发送后清空；Enter 发送 / ShiftEnter 换行
- **结果：** [ ]

### TC-C04 · P1 · 受控输入可用性
- **步骤：** 程序化填值 vs 真实键入
- **期望：** 真实键入可点发送（React 受控不丢事件）
- **结果：** [ ]

### TC-C05 · P1 · 启动 checklist / 失败卡
- **步骤：** 观察冷启动进度；人为制造超时/失败
- **期望：** 进度诚实；失败有可操作卡（非永久绿点）
- **结果：** [ ]

### TC-C06 · P1 · 未读与「跳转未读」
- **步骤：** 产生未读 → 在聊天点跳转；再在高级 Tab 看按钮
- **期望：** 聊天内滚动到未读；非聊天 Tab 应先回聊天再跳（不死按钮）
- **结果：** [ ]

### TC-C07 · P1 · 过滤 / 刷新 / 血缘跳转
- **期望：** 过滤生效；刷新不丢；点 reply 血缘能定位父消息
- **结果：** [ ]

### TC-C08 · P1 · NeedsYouBar
- **步骤：** 制造 error / handoff_missing
- **期望：** 条出现；点击开抽屉；**stalled 不进**此条（auto-nudge 另测）
- **结果：** [ ]

### TC-C09 · P1 · Auto-nudge stalled
- **步骤：** 制造 stalled（有未读+长期无活动）
- **期望：** 静默 wake（日志/邮箱可见）；有冷却；不误伤忙碌 agent
- **注意：** 2026-08 实测 spawn 后立刻出现 `manual wake`——若复现记 FAIL/UX
- **结果：** [ ]

### TC-C10 · P1 · 手动 ⚡ wake
- **步骤：** 抽屉或 ⌘K 确认后 wake
- **期望：** mailbox wake + 引擎 kick；Stop hook 不双烧一轮（消费正确）
- **结果：** [ ]

### TC-C11 · P1 · ModelPicker
- **期望：** tier/effort 写入方向；haiku effort 有提示；切换生效于后续 spawn
- **结果：** [ ]

### TC-C12 · P1 · 打断 / 打断全部
- **期望：** 进行中可打断；有排队提示；状态回闲
- **结果：** [ ]

### TC-C13 · P2 · SpellsLauncher
- **期望：** 列 spell；无 workspace 时拦 400；运行有反馈
- **结果：** [ ]

### TC-C14 · P1 · 黑板（若 UI 暴露）
- **期望：** 读/写/不覆盖新建/删确认/历史；审批 key 门可用
- **结果：** [ ]

### TC-C15 · P0 · WS 实时
- **期望：** 新消息/agent_state 无需手动刷即出现；断线有离线态
- **结果：** [ ]

---

## D. 高级 Tab

### TC-D01 · P0 · 默认只露聊天
- **步骤：** 清 `localStorage.swarmx.workspace.advancedTabs` 后进空间
- **期望：** 仅「聊天」+「高级」；无协作图/台账/竞赛…常驻
- **结果：** [ ]

### TC-D02 · P1 · 展开持久化
- **步骤：** 展开高级 → 刷新
- **期望：** 仍展开；折叠后刷新保持折叠
- **结果：** [ ]

### TC-D03 · P1 · 深链自动展开
- **步骤：** 直开 `/chat/:id/dag`
- **期望：** 高级展开且 DAG 可见
- **结果：** [ ]

### TC-D04 · P1 · DAG
- **前置：** 有活队长
- **期望：** 节点/边；点节点开抽屉；方向过滤正确；空态有去聊天 CTA
- **结果：** [ ]

### TC-D05 · P1 · Ledger
- **期望：** task/progress 双栏；黑板写入后增量更新；空态可读
- **结果：** [ ]

### TC-D06 · P1 · 💰 Fusion 表单
- **步骤：** 发起竞赛打开表单；填需求（可不提交全赛）
- **期望：** autopilot 默认；校验 ≥2 引擎；git 提示可读
- **结果：** [ ]

### TC-D07 · P2 · 💰💰 Fusion 全赛
- **步骤：** 短需求 + 检查命令跑完全流程
- **期望：** 并行方向；judge/synthesize/decide；不双开 judge
- **结果：** [ ]

### TC-D08 · P1 · 💰 Consult
- **步骤：** 选 2–3 模型；问极简题；开始会诊
- **期望：** panel 答案 + 共识/分歧 + 综合定稿；成本提示可见；部分失败可展示
- **结果：** [ ]

### TC-D09 · P1 · Replays 列表与播放
- **前置：** 有过 spawn
- **期望：** 列表有条目；播放器可播；Esc 回；可下载 .cast
- **结果：** [ ]

### TC-D10 · P2 · `/context` 重定向
- **期望：** 进 ledger，不 404
- **结果：** [ ]

### TC-D11 · P2 · ⌘1–6 切 Tab
- **期望：** 快捷键切视图；输入框内不误触
- **结果：** [ ]

---

## E. AgentDrawer

### TC-E01 · P1 · 打开/关闭与深链
- **步骤：** `?agent=` 打开；关清 query
- **期望：** 活默认 Activity；死默认 Recordings
- **结果：** [ ]

### TC-E02 · P1 · 终端 Tab（活）
- **期望：** PTY 双向；可见输出
- **结果：** [ ]

### TC-E03 · P0 · 死 agent 不连 PTY
- **步骤：** kill 后开抽屉终端
- **期望：** **不**建 `/ws/pty`；有明确死态
- **结果：** [ ]

### TC-E04 · P1 · wake / pause / resume / kill
- **期望：** 确认框；动作后状态正确
- **结果：** [ ]

### TC-E05 · P2 · Activity / Messages / Recordings / Context
- **期望：** 各 Tab 有数据或诚实空态
- **结果：** [ ]

### TC-E06 · P1 · reasonix 默认非 PTY Tab
- **期望：** 默认 Activity（HTTP 引擎）
- **结果：** [ ]

---

## F. 侧栏全局页

### TC-F01 · P1 · 文件浏览器
- **期望：** 列目录；读文本/图；上钻；切 ws 重置；browse-all 越狱边界 403
- **结果：** [ ]

### TC-F02 · P1 · 本机终端
- **期望：** **不自动连**；点连接才建 WS；断线可重连；切 ws 会话合理
- **结果：** [ ]

### TC-F03 · P1 · MCP 管理
- **期望：** 探测 node/npm/uv；开关写用户级配置；密钥 dialog 可用
- **结果：** [ ]

### TC-F04 · P1 · 定时 Cron
- **步骤：** 新建 → 列表出现；可立即运行（💰 若会打队长）；删/禁用
- **期望：** 时区显示正确；enable 防重入；失败回滚
- **结果：** [ ]

### TC-F05 · P1 · 目标 Goals
- **步骤：** 创建 → 状态流转 → 完成/归档
- **期望：** 校验预算/标准；后端失败≠空列表撒谎
- **结果：** [ ]

### TC-F06 · P1 · 任务 Tasks
- **期望：** Kanban；override；archive/reopen 确认；写失败回滚
- **结果：** [ ]

### TC-F07 · P1 · 用量 Usage
- **期望：** 有过 💰 会话后有数据；价目可编/保存/恢复默认（确认）
- **结果：** [ ]

### TC-F08 · P1 · 通知
- **期望：** 铃铛预览；全部已读确认；跳源空间；过滤 noisy wake
- **结果：** [ ]

### TC-F09 · P2 · ⌘K 命令面板
- **期望：** 新建/导航/设置/wake；**缺 fusion/consult 项则记 UX 债**
- **结果：** [ ]

### TC-F10 · P2 · 侧栏折叠持久化
- **结果：** [ ]

### TC-F11 · P2 · Debug 路由
- **期望：** 生产进 `/chat`；dev 才可用
- **结果：** [ ]

---

## G. 设置

### TC-G01 · P1 · General
- **期望：** 语言即时切；桌面通知/启动行为（Tauri）；killOthersOnFail 开关
- **结果：** [ ]

### TC-G02 · P1 · Appearance
- **结果：** [ ]

### TC-G03 · P2 · Shortcuts 只读列表
- **结果：** [ ]

### TC-G04 · P1 · Models 脏离开确认
- **步骤：** 改模型不保存 → 切 section / 离开
- **期望：** 确认守卫；保存/失败 toast
- **结果：** [ ]

### TC-G05 · P0 · Plugins 引擎卡
- **期望：** 已装/可用/版本/路径；未装有一键装+文档；探针四态
- **结果：** [ ]

### TC-G06 · P1 · Zulu license
- **期望：** 保存本机；模型列表加载；未配时 Consult 引导
- **结果：** [ ]

### TC-G07 · P1 · Privacy 清 localStorage
- **期望：** 确认后清 `swarmx:*`；不可静默
- **结果：** [ ]

### TC-G08 · P2 · About / 更新
- **期望：** 版本正确；Tauri 检查更新路径
- **结果：** [ ]

### TC-G09 · P2 · `/settings/:section` 深链
- **期望：** 无效 section → general
- **结果：** [ ]

---

## H. 失败与韧性

### TC-H01 · P0 · 后端挂了 UI 诚实
- **结果：** [ ]（见 A02）

### TC-H02 · P1 · spawn/init/wake HTTP 失败
- **期望：** toast；不出现「在线但永远无消息」的假绿
- **结果：** [ ]

### TC-H03 · P1 · 队长起不来仍可进群
- **期望：** 失败卡 + 可重试/换引擎
- **结果：** [ ]

### TC-H04 · P1 · hang vs `.error` 自愈
- **期望：** hang→stalled/nudge；producer 死写 `.error`→下游可醒
- **结果：** [ ]

### TC-H05 · P0 · 计费 fallback 必 toast
- **步骤：** 强制落到 API-billed 引擎
- **期望：** 可见 billing_surface 提示；不可静默烧钱
- **结果：** [ ]

### TC-H06 · P1 · Claude 禁 ambient ANTHROPIC_*
- **期望：** 默认不把 API key 环境带进订阅路径子进程
- **结果：** [ ]

### TC-H07 · P1 · killOthersOnFail
- **期望：** 开时一个挂带走同编排；关时独立
- **结果：** [ ]

### TC-H08 · P1 · WS 断线恢复
- **期望：** swarm / PTY / terminal 各自可恢复或不撒谎
- **结果：** [ ]

### TC-H09 · P1 · 路径安全
- **期望：** 黑板穿越 400；files jail 403
- **结果：** [ ]

### TC-H10 · P2 · 未保存离开（Models / 价目 / 黑板）
- **结果：** [ ]

---

## I. 多引擎（每引擎最少一条）

对 **claude / codex / opencode / reasonix / zulu / kimi** 各跑：

### TC-I01–I06 · P1 · 💰 作队长端到端
- **步骤：** 该引擎为队长 → ready → MCP 工具可见 → 短任务 reply
- **期望：** 计费面标签正确；输入策略符合引擎（PTY / TUI / serve）
- **结果：** claude[ ] codex[ ] opencode[ ] reasonix[ ] zulu[ ] kimi[ ]

### TC-I07–I12 · P1 · 💰 作 worker
- **步骤：** 队长 spawn 该引擎 worker → 收信 → wake → 回写
- **结果：** claude[ ] codex[ ] …（同上）

### TC-I13 · P1 · 未装空态 / 一键装（适用者）
- **结果：** [ ]

### TC-I14 · P1 · Fallback 链 toast
- **结果：** [ ]

---

## J. 装包真机 📦

### TC-J01 · P0 · 下载→安装→打开→新建空间
- **期望：** 零命令；init 可用
- **结果：** [ ]

### TC-J02 · P0 · CWD=/ 无环境变量仍跑
- **期望：** spells/roles/plugins builtin 生效
- **结果：** [ ]

### TC-J03 · P1 · sidecar 三二进制 + opencode wake JS
- **结果：** [ ]

### TC-J04 · P1 · 三平台 PATH/HOME/可执行后缀
- **结果：** mac[ ] win[ ] linux[ ]

### TC-J05 · P2 · 更新通道 / openMainOnLaunch / 桌面通知
- **结果：** [ ]

---

## K. 回归锁（改架构必跑）

### TC-K01 · P0 · TurnDelivery：reasonix wake 单入口
- **期望：** 不存在「inject 拒 reasonix / kick 必须走 serve」双规则；wake 成功或原子失败不丢信
- **结果：** [ ]

### TC-K02 · P0 · PTY wake 消费 mailbox（防双烧）
- **期望：** 一次 wake = 一回合；Stop hook 不二次 block
- **结果：** [ ]

### TC-K03 · P1 · Bootstrap：ShimReady→mcp-ready→paste
- **期望：** 冷启动注入成功；大 prompt 可提交（settle 缩放）
- **结果：** [ ]

### TC-K04 · P1 · 录像含生命周期 OSC，首帧不是误 wake（若再出现记 FAIL）
- **结果：** [ ]

---

## A′. 冷启动加深（补 A13+）

### TC-A13 · P1 · CreateWizard 多 root
- **步骤：** 主项目 + 依赖/工具目录一并选入
- **期望：** roots 写入；侧栏多 root 树可见；files jail 仍不越界
- **结果：** [ ]

### TC-A14 · P1 · 路径即时校验
- **步骤：** 填不存在 / 不可读 / 非目录路径
- **期望：** 提交前可读错误；不半创建空间
- **结果：** [ ]

### TC-A15 · P1 · 自选 captain_cli + fallback toast
- **步骤：** 选不可用引擎为队长（或强制 fallback）
- **期望：** `notifySpawnFallbacks` 类 toast 可见；billing_surface 诚实
- **结果：** [ ]

### TC-A16 · P2 · root-suggestions
- **期望：** 有合理推荐或诚实空；不推荐越狱路径
- **结果：** [ ]

### TC-A17 · P1 · 📦 Tauri 选文件夹 vs 浏览器粘贴路径
- **期望：** 桌面有选目录；纯浏览器路径可用且文案不撒谎
- **结果：** [ ]

---

## B′. 方向/合并加深（补 B11+）

### TC-B11 · P0 · merge 冲突可读
- **前置：** isolated 方向与 main 制造冲突
- **步骤：** 合并到主线
- **期望：** 冲突 UI 可读；worktree 不丢；可重试；不假成功
- **结果：** [ ]

### TC-B12 · P1 · thread diff 预览再合并
- **期望：** 先看 diff，再确认合并
- **结果：** [ ]

### TC-B13 · P1 · 💰 swarm_name_thread 改名同步
- **步骤：** 队长命名方向
- **期望：** 侧栏标题 + URL slug 同步；刷新不丢
- **结果：** [ ]

### TC-B14 · P1 · preparing→ready 失败 → degraded
- **期望：** 失败态可操作恢复；不卡「永远 preparing」
- **结果：** [ ]

### TC-B15 · P2 · 侧栏管理 roots：增删
- **期望：** 增删后 files/jail 立即一致
- **结果：** [ ]

---

## C′. 聊天/Composer 加深（补 C16+）

### TC-C16 · P0 · 粘贴/拖拽图片附件
- **步骤：** 粘贴或拖图进 Composer → 发送
- **期望：** `/api/attachment` 成功；气泡可见图；队长侧可读
- **结果：** [ ]

### TC-C17 · P1 · 附件上传失败禁止发送
- **期望：** 失败态不可点发送；错误可读
- **结果：** [ ]

### TC-C18 · P1 · prompt optimize
- **步骤：** 点优化草稿；可取消；制造失败
- **期望：** 改写进框；失败 toast；不吞草稿
- **结果：** [ ]

### TC-C19 · P1 · PlanStickyCard
- **步骤：** 黑板出现 plan 约定内容
- **期望：** 粘性卡出现/更新/消失正确
- **结果：** [ ]

### TC-C20 · P2 · PulseRail
- **期望：** 成员脉搏可读；点开进抽屉
- **结果：** [ ]

### TC-C21 · P2 · 代码块一键复制
- **期望：** 复制成功；clipboard 拒权时不装死
- **结果：** [ ]

### TC-C22 · P1 · BootstrapChecklistCard / OrchestratorFailureCard
- **期望：** 冷启动进度诚实；失败卡可重试/换引擎
- **结果：** [ ]

### TC-C23 · P1 · 加深 C08：NeedsYou 三态分测
- **步骤：** 分别制造 error / handoff_missing / stalled
- **期望：** 仅前两者进 NeedsYouBar；排序正确；文案不混
- **结果：** [ ]

---

## D′. Fusion / Consult 加深（补 D12+）

### TC-D12 · P1 · 💰 Fusion 非 autopilot 手选 2–4 引擎
- **结果：** [ ]

### TC-D13 · P1 · 💰 check_cmd 门禁失败标记 contestant
- **结果：** [ ]

### TC-D14 · P1 · judge 已 running 刷新不双开
- **结果：** [ ]

### TC-D15 · P1 · needs_decision 人工选赢家 + decide
- **结果：** [ ]

### TC-D16 · P1 · synthesize vs 纯 judge 路径差异可见
- **结果：** [ ]

### TC-D17 · P1 · Consult 无/有 Zulu license 矩阵
- **期望：** 未配 → 引导设置；已配 → 模型列表加载
- **结果：** [ ]

### TC-D18 · P2 · Consult 部分 panel 失败仍出综合
- **结果：** [ ]

---

## E′. 生命周期 / Reaper（补 E07+）

### TC-E07 · P1 · kill → recording finalize → Replays 可播完整结尾
- **结果：** [ ]

### TC-E08 · P1 · 无 ShimExit 时 reaper 合成退出
- **期望：** UI 非永生绿点
- **结果：** [ ]

### TC-E09 · P1 · 服务重启 orphan pid 清理
- **期望：** 可选 auto-respawn orchestrator 行为符合配置且可感知
- **结果：** [ ]

### TC-E10 · P2 · pause/resume：PTY vs HTTP 引擎语义
- **结果：** [ ]

### TC-E11 · P2 · retention_days 清旧（运维向）
- **结果：** [ ]

---

## F′. Goals / Cron / Tasks / 通知加深（补 F12+）

### TC-F12 · P1 · Goal evidence 列表/添加与 status 联动
- **结果：** [ ]

### TC-F13 · P1 · Goal token budget 校验
- **结果：** [ ]

### TC-F14 · P1 · Goal 按 thread 过滤
- **结果：** [ ]

### TC-F15 · P1 · Cron 非法表达式 preview + 时区 + toggle 防重入
- **结果：** [ ]

### TC-F16 · P1 · 💰 Cron run 触发队长真实回合
- **结果：** [ ]

### TC-F17 · P2 · Tasks 跨 workspace 过滤 + 写失败回滚
- **结果：** [ ]

### TC-F18 · P2 · Debug 多 PTY 网格（仅 dev）
- **期望：** spawn/kill/maximize；不污染默认用户空间
- **结果：** [ ]

### TC-F19 · P1 · NotificationPopover → 全部页跳源空间
- **结果：** [ ]

### TC-F20 · P1 · Usage 价目脏离开确认（对照 G04）
- **结果：** [ ]

### TC-F21 · P1 · MCP install/uninstall + 密钥 dialog 全路径
- **结果：** [ ]

---

## G′. i18n / 快捷键 / 焦点（补 G10+）

### TC-G10 · P1 · 全产品 zh↔en
- **期望：** 侧栏/向导/错误/NeedsYou/高级 Tab **无大片漏翻**
- **结果：** [ ]

### TC-G11 · P2 · Replay 播放器快捷键（空格/←→/./Esc）
- **结果：** [ ]

### TC-G12 · P2 · Dialog/Sheet Esc 与焦点回 composer
- **结果：** [ ]

### TC-G13 · P2 · Settings shortcuts 列表与真实绑定一致
- **结果：** [ ]

---

## H′. 安全 / 计费加深（补 H11+）

### TC-H11 · P0 · fallback→API-billed 场景矩阵必 toast
- **步骤：** 订阅不可用被迫 API；多引擎链上每一跳
- **期望：** 每跳可见；UI 标签同步；不可静默烧钱
- **结果：** [ ]

### TC-H12 · P1 · blackboard delete + compact + `..` 穿越 400
- **结果：** [ ]

### TC-H13 · P1 · files `all=1` 越狱 403
- **结果：** [ ]

### TC-H14 · P2 · `/api/debug/log` 仅开发可接受
- **结果：** [ ]

---

## I′. 引擎特异投递（拆开笼统 I 域）

### TC-I15 · P0 · 💰 opencode 队长：大 bootstrap 经 `/tui/submit` 真开跑
- **期望：** 非键击假成功；冷启动重试可感知
- **结果：** [ ]

### TC-I16 · P0 · 💰 reasonix：serve → submit → SSE turn_done → 再读信
- **结果：** [ ]

### TC-I17 · P1 · 💰 zulu：独立 HOME + session SSE；Activity 有 tool 镜像、无 .cast
- **结果：** [ ]

### TC-I18 · P1 · 各引擎「录像有/无」与抽屉默认 Tab 一致
- **结果：** [ ]

### TC-I19 · P1 · Plugins 一键装：SSE 日志流 → 卡翻 **usable**（非只 installed）
- **结果：** [ ]

### TC-I20 · P1 · probe 进行中 / 缓存 verdict / 手动刷新
- **结果：** [ ]

### TC-I21 · P1 · kimi wake hook-format / exit 语义
- **结果：** [ ]

---

## J′. 装包桌面加深（补 J06+）

### TC-J06 · P1 · 📦 桌面通知开关真实弹系统通知
- **结果：** [ ]

### TC-J07 · P1 · 📦 openMainOnLaunch / 更新检查 + 设置红点
- **结果：** [ ]

### TC-J08 · P2 · 📦 标题栏拖区不挡按钮
- **结果：** [ ]

---

## L. Handoff / 多 agent（**产品核心，原套件最大洞**）

### TC-L01 · P0 · 💰 list_roles → spawn_worker(role) 起真实 worker
- **步骤：** 队长派一个明确 role；用户看 DAG/抽屉
- **期望：** 新成员上线；role 正确；可收信
- **结果：** [ ]

### TC-L02 · P0 · 💰 worker 写 mint handoff key → 队长被 wake → ledger 更新
- **期望：** 依赖交付后队长续跑；task/progress 可见
- **结果：** [ ]

### TC-L03 · P0 · worker 死且未交付 → handoff_missing → NeedsYou
- **期望：** 条出现且文案是 handoff（不是 stalled）
- **注意：** 2026-08 走测：正常 teardown/`kill -9` 都会写 `<key>.error` → 只有 `handoff_failed`，**NeedsYou 不出现**（见 UX-016）。本条对「真·silent exit 无 .error」仍有效；对「用户杀 worker」当前产品 **FAIL**
- **结果：** [!]

### TC-L04 · P1 · 写 `<key>.error` → 下游依赖解除 / 队长醒
- **结果：** [ ]

### TC-L05 · P1 · 同方向同 role 第二活 worker 被拒；consumes 环被拒
- **期望：** 拒收可读；**禁止**静默覆盖 key
- **结果：** [ ]

### TC-L06 · P1 · handoff 写完后 auto-kill（约 5s）
- **期望：** UI 变死；录像 finalize；不永生绿
- **结果：** [ ]

### TC-L07 · P1 · 💰 多 worker 并行 + reviewer consumes
- **期望：** 边出现在 DAG；交付顺序符合 consumes
- **结果：** [ ]

### TC-L08 · P2 · roles catalog 与内置 8 role 一致可 spawn
- **结果：** [ ]

### TC-L09 · P1 · 黑板写订阅 key → push wake（mailbox + PTY kick）
- **期望：** 已停下游当场醒；无死锁轮询
- **结果：** [ ]

### TC-L10 · P1 · 用户可见：spawn 后 DAG 边出现、ledger 增量（因果，非空态）
- **结果：** [ ]

---

## M. MCP 工具面 / wake-check

### TC-M01 · P0 · 队长回合能列出全部 swarm MCP tools
- **期望：** send/list/search messages、agents、blackboard CRUD、spawn_worker、list_roles、name_thread、fusion_consult 等齐全；无已删 spell tools
- **结果：** [ ]

### TC-M02 · P0 · wake-check：有未读 wake → block 续跑；无则停
- **结果：** [ ]

### TC-M03 · P1 · list_messages 中途不消费 wake（防假死）
- **结果：** [ ]

### TC-M04 · P1 · kimi `--hook-format kimi` / exit 2 block
- **结果：** [ ]

### TC-M05 · P1 · 📦 opencode wake JS：有则生效；无则降级可感知不崩
- **结果：** [ ]

### TC-M06 · P2 · search_messages + in_reply_to 血缘
- **结果：** [ ]

### TC-M07 · P2 · agent 调 swarm_fusion_consult（非仅 UI）
- **结果：** [ ]

### TC-M08 · P1 · consume_wakes 与 Stop hook 协作不双烧
- **结果：** [ ]

---

## N. 用量 / 价目（估价，不是 BillingSurface 红线）

> 决策背景见 [usage-pricing-vs-cc-switch-2026-08.md](../research/usage-pricing-vs-cc-switch-2026-08.md)。  
> **红线**（禁静默 API）仍用 H05/H06/H11；本域只验 **token 刮取 + 价目 + `/usage` UI**。

### TC-N01 · P0 · 💰 Claude 跑后 Usage 有 events + 估算金额
- **步骤：** 短任务产生 usage → 开 `/usage`
- **期望：** 有 events；有估算 cost（或诚实 tokens-only）；不是空态撒谎
- **结果：** [ ]

### TC-N02 · P1 · 未知模型 → tokens only + 总成本 `≥`
- **期望：** `priced=false` 时总额带「≥」语义；不假装精确账单
- **结果：** [ ]

### TC-N03 · P1 · 改价目 Save → 金额变；Reset → 回 default
- **步骤：** 改一条 rate 保存；刷新；再重置确认
- **期望：** 金额随规则变；Reset 删用户配置回内置；path 显示为 `~/.swarmx/pricing.json`（Win 用 USERPROFILE）
- **结果：** [ ]

### TC-N04 · P1 · 价目脏离开确认
- **期望：** 与 F20/H10 一致；不可静默丢编辑
- **结果：** [ ]

### TC-N05 · P0 · 信任文案：估算 ≠ 订阅/套餐账单
- **期望：** UI 明示 API list 估算；Claude Max/订阅路径不装成「你花了 $x 现金」
- **结果：** [ ]

### TC-N06 · P1 · 引擎采集矩阵
- **期望：** Claude/Codex/Kimi 有数或可解释；opencode/reasonix/zulu **明示未采集**（禁止空白假装「你没花钱」）
- **结果：** [ ]

### TC-N07 · P1 · 📦 Windows/mac 价目路径
- **期望：** 装包 CWD=/ 时 Save/Reset 仍落用户家目录，不写幽灵相对路径
- **结果：** [ ]

### TC-N08 · P2 · 发版：litellm snapshot 新鲜度
- **步骤：** `node scripts/update-litellm-pricing.mjs --check`
- **期望：** 发版流程要求通过或有意落后并记录；CI 最终应接线（现状：脚本有、workflow 无 → 记 FAIL/债）
- **结果：** [ ]

### TC-N09 · P0 · 回归：现行 Opus（如 `claude-opus-4-8`）按 ~$5/$25，不得套 legacy $15/$75
- **期望：** 与现行 API list / LiteLLM 同量级；禁止裸 `opus` substring 把旗舰估成老价
- **结果：** [ ]

### TC-N10 · P1 · haiku / sonnet / gpt-5.2 代际价不离谱
- **期望：** haiku 现行价；`gpt-5.2` 不被粗 `gpt-5` 错价；sonnet 现行档正确
- **结果：** [ ]

### TC-N11 · P1 · LiteLLM fallback 真生效
- **步骤：** 选一个 primary 都不匹配、表里有的模型 id（如 gemini）
- **期望：** 有价；UI 显示 fallback 模型数；英文 UI 不硬编码整句中文（曾有 i18n bug）
- **结果：** [ ]

### TC-N12 · P1 · DB/查询失败 ≠ 「暂无用量」空态
- **期望：** 后端错误有 error UI；不吞成 emptyHint
- **结果：** [ ]

### TC-N13 · P2 · 趋势图是「最近 N 天」不是「最早 N 天」
- **结果：** [ ]

### TC-N14 · P2 · 零价 LiteLLM 行不标成「已精确定价」
- **期望：** input=output=0 的表项不假装可靠账单
- **结果：** [ ]

### TC-N15 · P1 · LiteLLM 进程内自动刷新（学 cc-switch）
- **步骤：** 起服务（勿设 `SWARMX_DISABLE_LITELLM_REFRESH`）→ 看日志 / `GET /api/usage/pricing` 的 `fallback.origin`
- **期望：** 成功时 `origin=refreshed`（或先 `disk` 再刷新）；失败时仍可用 embedded/disk；写 `~/.swarmx/litellm_pricing.json`；设 disable 后 `auto_refresh=false` 且不拉网
- **结果：** [ ]

---

## 产品面 ↔ 域（写完自检：禁止再漏整块）

| 产品面 | 域 |
|---|---|
| 冷启动 / Welcome / CreateWizard / 多 root | A + A′ |
| 空间 / 方向 / worktree / 合并冲突 | B + B′ |
| 聊天 / 附件 / optimize / NeedsYou / wake | C + C′ |
| DAG / Ledger / Fusion / Consult / Replays | D + D′ |
| AgentDrawer / kill / reaper / 录像 finalize | E + E′ |
| 侧栏全局页（files/terminal/mcp/cron/goals/tasks/usage壳） | F + F′ |
| 设置 / i18n / 快捷键 | G + G′ |
| 失败 / 计费红线 / 路径安全 | H + H′ |
| 六引擎 + 特异投递 | I + I′ |
| 装包 / 桌面 | J + J′ |
| TurnDelivery / wake 双烧锁 | K |
| **Handoff / spawn_worker / 多 agent** | **L** |
| **MCP tools / wake-check** | **M** |
| **用量估价 / 价目 / LiteLLM** | **N** |
| BillingSurface 红线（禁静默 API） | H05/H06/H11（勿与 N 混） |

若上表某行在 SUITE 无对应用例 → **套件未写完**，禁止开跑宣称完整。

---

## 已知浅勾选（跑时必须拆开，禁止「进过页就 PASS」）

| 原 TC | 必须拆开验 |
|---|---|
| C01–C02 | 单队长 reply ≠ 蜂群；**L01–L03 同轮必跑** |
| C08 | 用 C23 三态分测 |
| C14 黑板 | 用 L02/L09 + H12；Drawer/Ledger/compact |
| B08 合并 | 用 B11–B12 冲突+diff |
| D06–D08 | 用 D12–D18 分支 |
| E04–E05 | 用 E07–E10 + I18 |
| F03–F07 | 用 F12–F21；**F07 价目用 N 域** |
| G05–G06 | 用 I19–I20 + D17 |
| I01–I14 | 用 I15–I21 引擎特异标准 |
| K01–K04 | 必须带**用户可见**断言（醒了、不双烧、录像不脏） |
| F07 Usage | **禁止只勾「有数据」**；必须跑 N01/N05/N09 |

---

## 统计

| 域 | 条数（约） |
|---|---|
| A / A′ 冷启动 | 17 |
| B / B′ 空间/方向 | 15 |
| C / C′ 聊天 | 23 |
| D / D′ 高级 | 18 |
| E / E′ 抽屉/生命周期 | 11 |
| F / F′ 侧栏页 | 21 |
| G / G′ 设置/i18n | 13 |
| H / H′ 失败/安全 | 14 |
| I / I′ 多引擎 | 21+ |
| J / J′ 装包 | 8 |
| K 架构锁 | 4 |
| L Handoff/多 agent | 10 |
| M MCP/wake-check | 8 |
| **N 用量/价目** | **15** |

**合计约 200+ 条**（I 按 6 引擎展开后更多）。

**每轮最低：** 全部 **P0**（含 **L01–L03、M01–M02、C16、B11、H11、I15–I16、N01、N05、N09**）+ 本周改动域的 P1。  
**禁止：** 只勾 A–K「进过页面」就宣称完整验收。  
**开跑门闩：** 上表「产品面 ↔ 域」无空行，且 research 价目决策已读。
