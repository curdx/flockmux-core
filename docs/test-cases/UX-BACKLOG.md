# UX 体验债（测着用不爽 → 优化候选）

规则：测用例时像用户。不爽就记一条。**不要在测的时候顺手大改 UI**——先记这里，再开 PR。

格式：

```
### UX-<id> · <一句话痛点>
- 来源：TC-xxx / 日期 / 谁测的
- 场景：…
- 不爽：…
- 建议：…
- 优先级：P0|P1|P2
- 状态：open|wontfix|fixed
```

---

## 已登记（2026-08 真机 + 走查）

### UX-001 · spawn 后立刻出现「manual wake」噪声
- 来源：TC-C09 / 2026-08 真机
- 场景：新建空间拉起队长后，录像/系统卡很快出现 manual wake
- 不爽：用户没点过 ⚡，却像「系统自己戳了一下」；难判断是正常 bootstrap 还是真 stalled
- 建议：区分 bootstrap kick vs user/auto-nudge wake 的文案与系统卡类型；bootstrap 不写「manual」
- 优先级：P1
- 状态：open

### UX-002 · 程序化填 Composer 点不了发送
- 来源：TC-C04 / 自动化踩坑
- 场景：自动化 `fill()` 输入框后发送按钮仍 disabled
- 不爽：真实用户键入没事；脚本/无障碍辅助若走 value 赋值会翻车
- 建议：发送启用条件跟 visible value 对齐，或文档明确「必须发 input 事件」；e2e 用 `pressSequentially`
- 优先级：P2
- 状态：open

### UX-003 · 高级 Tab 藏太深但深链/快捷键仍假设常驻
- 来源：TC-D01–D03 / TC-C06
- 场景：默认只露聊天后，「跳转未读」在非聊天 Tab、⌘1–6、旧书签仍可能指向高级视图
- 不爽：按钮看起来能点，实际要先展开高级或先回聊天；localStorage 钉死后聊天永久六 Tab
- 建议：未读跳转强制 `navigate` 到 chat；深链自动展开；离开高级路由后收起；不持久化展开
- 优先级：P1
- 状态：fixed（2026-08：session-only expand；回聊天自动收起）

### UX-004 · ⌘K 缺 Fusion / Consult 入口
- 来源：TC-F09
- 场景：键盘党想开竞赛/会诊
- 不爽：侧栏/高级有，命令面板没有 → 功能发现靠运气
- 建议：补 palette actions；或明确「仅高级 Tab」并在空态提示
- 优先级：P2
- 状态：fixed（2026-08：WORKSPACE_VIEWS 对齐 buildTabs）

### UX-005 · Consult / Fusion 成本感知偏弱
- 来源：TC-D06–D08
- 场景：一点「开始」就多引擎烧额度
- 不爽：有提示但仍像「普通按钮」；失败半截时钱已花
- 建议：二次确认 + 预估模型数 × 单价；部分失败时汇总已花
- 优先级：P1
- 状态：open

### UX-006 · 引擎「已安装」绿标 ≠ 能干活
- 来源：TC-A08–A09 / TC-G05
- 场景：装了 CLI 但未登录 / PATH 在 GUI 里找不到
- 不爽：用户以为能用，spawn 才爆
- 建议：卡片四态强制：missing / installed / needs_login / usable；绿只用 usable
- 优先级：P0
- 状态：open

### UX-007 · 本机终端「连接」门槛对新手不直观
- 来源：TC-F02
- 场景：进终端页空白，要先点连接
- 不爽：像坏了，不像「按需连」
- 建议：空态大字说明「不会自动连，避免占 shell」+ 一键连接 CTA
- 优先级：P2
- 状态：open

### UX-008 · 新建向导 / 扫描等待焦虑
- 来源：TC-A06
- 场景：提交后扫描转圈
- 不爽：不知道能不能跳、跳了队长还在不在干
- 建议：跳过文案写清「进群后后台继续」；超时自动进群
- 优先级：P2
- 状态：open

### UX-009 · NeedsYou vs stalled 用户心智混
- 来源：TC-C08–C09
- 场景：条不出现但后台 auto-nudge；或条出现但用户以为要手动点
- 不爽：不知道系统已经在救 / 还要不要管
- 建议：UI 明示「已自动轻推 N 次」；NeedsYou 只留给真要人手的事
- 优先级：P1
- 状态：open

### UX-010 · README / 首屏价值主张仍易飘宽
- 来源：adoption 评审（非单测）
- 场景：新用户打开仓库或下载页
- 不爽：功能清单太长，不知道「只为多 CLI 蜂群」
- 建议：下载 CTA + 一句蜂群主张守住；源码构建永远二级
- 优先级：P1
- 状态：open（README 已改一轮，装包首屏再验）

---

### UX-011 · 验收清单曾漏掉「蜂群」本身
- 来源：SUITE 自审 / 2026-08
- 场景：只跑 A–K「进过聊天页」就觉得测完了
- 不爽：产品卖点是多 agent + handoff，测集却像单聊壳
- 建议：每轮 P0 强制含 L01–L03；禁止浅勾选见 SUITE 末表
- 优先级：P0
- 状态：open（文档已补；执行纪律待守）

### UX-012 · Usage 曾把现行 Opus 估成 legacy $15/$75
- 来源：TC-N09 / 2026-08
- 场景：裸 `opus` substring 盖住 `claude-opus-4-8` 等旗舰 id
- 不爽：估算像假账单；订阅路径更误导
- 建议：最长匹配 + 现行/legacy 分规则；刷 LiteLLM；文案标估算≠订阅账单
- 优先级：P0
- 状态：fixed（2026-08：longest-match + opus-4-8 档 $5/$25 + snapshot 刷新）

### UX-013 · 未采集引擎的 Usage 空白像「没花钱」
- 来源：TC-N06
- 场景：opencode / reasonix / zulu 无 transcript usage
- 不爽：空态与「真没跑」不可分
- 建议：按引擎标「未采集」；有数才显示金额
- 优先级：P1
- 状态：open

---

### UX-014 · Usage 模型 id 出现 `claude-opus-5`
- 来源：TC-N01 / 2026-08 走测
- 场景：按模型表显示 `claude-opus-5`（非 4-8 等）
- 不爽：看不出对应哪代；价目匹配可能漂到 LiteLLM/未列出
- 建议：核对 transcript 刮取的 model 字段；UI 旁注规范化后的价目命中规则
- 优先级：P2
- 状态：open

---

### UX-015 · 合并进行中/主线 dirty 时对话框撒谎「无需合并」
- 来源：TC-B11 / 2026-08 走测
- 场景：已 POST merge → main 处于合并中/脏；再开「合并到主线」
- 不爽：文案变成「这个方向还没有改动，无需合并」，掩盖冲突/进行中态（侧栏仍可能显示 ↑↓）
- 建议：diff 空但 `base_dirty` / MERGE_HEAD 时明示「主线合并进行中或有未提交改动」，不要走 `noChanges` 文案
- 优先级：P1
- 状态：fixed（2026-08：`MergeDialog` 区分 `trulyNoChanges` vs `base_dirty`；新增 `merge.dirtyBlocksPreview`）

---

### UX-016 · worker 被杀/崩溃写了 `.error`，NeedsYou 却不收 handoff
- 来源：TC-L03 / 2026-08 走测
- 场景：`DELETE /api/agent` 或 `kill -9` 后 blackboard 有 `role.done.error`，`handoff_failed=true`，`handoff_missing=false`
- 不爽：NeedsYouBar 只认 `handoff_missing`；退出路径几乎总会写 `.error`，导致「没交付就死」进不了「需要你」条
- 建议：NeedsYou 同时收 `handoff_failed`（或 kill 不写 `.error` 仅标 missing——需产品二选一）；DAG 已显示 ✗ 但全局条空白
- 优先级：P0
- 状态：fixed（2026-08：`hasUndeliveredHandoff` + Shell `needsYouMembers`/`handoffMissingAgents` 同时收 failed；触控目标加大）

---

### UX-017 · 计划绿条「全部已交付」与 NeedsYou「没交结果」对撞
- 来源：UX 走测 / ui-ux-pro-max Feedback honesty
- 场景：`plan.json` 全勾 done，但 worker `handoff_failed`
- 不爽：绿条宣称交付完成，上方同时「请看一眼 · 没交结果」
- 建议：有未交付 handoff 时改琥珀文案，禁止绿条 allDone
- 优先级：P1
- 状态：fixed（2026-08：`PlanStickyCard` + `undeliveredHandoffs`）

### UX-018 · handoff 提示三连轰炸
- 来源：ui-ux-pro-max cognitive load / 2026-08
- 场景：NeedsYou + PlanSticky + composer 旁横幅同时说「没交结果」
- 不爽：同一事实说三遍，淹没真正要做的决定
- 建议：composer 横幅删除；NeedsYou 保留芯片 + 一行行动提示；Plan 只在 allDone 冲突时琥珀提示
- 优先级：P1
- 状态：fixed（2026-08）

### UX-019 · 双队长无法区分
- 来源：2026-08 走测（审计试用空间双 orchestrator）
- 场景：误 spawn 两个队长后，NeedsYou / PulseRail / 成员栏都只写「队长」
- 不爽：点哪个都不知道；错误态队长与健康队长长得一样
- 建议：同名角色并列时追加 agent_id 末 4 位；**根治：同方向最多一个 live orchestrator**
- 优先级：P1
- 状态：fixed（2026-08：`roleLabelAmong` + `run_spell`/`POST /api/agent` 幂等复用 + reaper 自动拆多余副本；唯一队长 soft-watchdog 不杀）

### UX-020 · 沉默重复队长烧额度，小白不知要清
- 来源：2026-08 / 产品原则「装完零命令」
- 场景：二次 init / 竞态拉出第二个 orchestrator，watchdog 只标红不杀
- 不爽：用户不懂「副本」；进程挂着烧订阅；NeedsYou 吵
- 建议：禁止第二 spawn；reaper 保留健康者、自动 teardown 其余
- 优先级：P0
- 状态：fixed（2026-08）

---

### UX-021 · NeedsYou「没交结果」点开是尸体 + 还能催
- 来源：2026-08 小白试用 / 审计试用空间
- 场景：worker 已被 DELETE/SIGKILL，条里仍「查看 修复工程师」
- 不爽：抽屉标「已结束」却聚焦「手动催一下」；小白不知该干嘛；`.error` 一直吵
- 建议：已退出不显示 Wake；handoff 芯片引导去跟队长说 + 「知道了」本会话收起
- 优先级：P0
- 状态：fixed（2026-08：AgentDrawer 隐藏 dead wake；NeedsYouBar dismiss + focus composer）

---

### UX-022 · 计划蓝卡片 + 思考蓝块把聊天顶出视口
- 来源：2026-08 样式审查 / 用户骂样式
- 场景：进行中计划默认展开全部步骤；思考中默认展开 swarm_list_* 工具墙
- 不爽：半屏蓝盒子，对话被挤没；小白看到一堆内部工具名
- 建议：计划默认一行进度+下一件，点开再全表；思考默认收起；死人 handoff 不进计划琥珀条
- 优先级：P0
- 状态：fixed（2026-08）

---

### UX-023 · 空房间状态条把引擎信息说三遍还催补齐
- 来源：2026-08 自测空 UI验证空间
- 场景：AI 未在线但已有可用引擎
- 不爽：条左侧列引擎、右侧「缺失 OpenCode」、空态再列一遍
- 建议：空房间只留 EmptyState；状态条不列引擎、不催补齐
- 优先级：P1
- 状态：fixed（2026-08）

### UX-024 · 用量页空态下面摊整墙价目编辑器
- 来源：2026-08 自测 /usage
- 场景：暂无用量时仍展示全部 spinbutton 价目表 + 双份免责声明
- 不爽：小白以为要先改价格才能用；页又长又像后台
- 建议：价目默认折叠为「高级」；免责只留 subtitle 一句
- 优先级：P1
- 状态：fixed（2026-08）

---

### UX-025 · 已结束 agent 抽屉仍显示「钩子 · 忙碌」
- 来源：2026-08 自测点「派给研究员」
- 场景：worker 已交付并退出，抽屉标「已结束」但页脚钩子仍「忙碌」
- 不爽：小白以为它还在干活；shim_ready 死后未清
- 建议：!live 时钩子/终端一律显示离线
- 优先级：P1
- 状态：fixed（2026-08）

### UX-026 · 协作图空态说「发起 spell」
- 来源：2026-08 自测 /dag
- 场景：无活跃 agent 时 EmptyState
- 不爽：小白不懂 spell；与聊天空态「发消息」不一致
- 建议：改成「回聊天发一条消息」
- 优先级：P2
- 状态：fixed（2026-08）

### UX-027 · 竞赛全自动无成本警示
- 来源：2026-08 自测 fusion 发起表单 / UX-005
- 场景：默认勾选全自动，一点就像普通按钮
- 不爽：多模型并行烧额度，新手无感知
- 建议：表单内琥珀提示「会同时拉多个模型」
- 优先级：P1
- 状态：fixed（2026-08：表单警示；二次确认仍可后续加）

### UX-028 · 已结束 agent 抽屉「已运行」继续涨
- 来源：2026-08 自测 agent drawer
- 场景：死后 header 仍用 now−spawned 显示「已运行」
- 不爽：像还在跑
- 建议：!live 用 killed_at 显示「存活」；无 killed_at 则只写「已结束」
- 优先级：P1
- 状态：fixed（2026-08）

### UX-029 · ahead=0 仍显示「合并到主线」→「无需合并」
- 来源：2026-08 浏览器自测合并冲突测 / dir-f4867d
- 场景：侧栏 ↓5（落后主线），工具栏仍有合并按钮；点开写「无需合并」
- 不爽：↓ 像待办，合并按钮是死胡同
- 建议：仅 ahead>0 显示合并 CTA；↓ tooltip 写清「不是待合并」
- 优先级：P0
- 状态：fixed（2026-08）

### UX-030 · DAG 空态三处复读「没有活跃 agent」
- 来源：2026-08 浏览器自测 /dag
- 场景：无 live agent 时左栏成员空文案 + 中栏 EmptyState + 右栏点节点
- 不爽：同一句话念三遍，中间大片空白
- 建议：空时只渲染单一 EmptyState
- 优先级：P2
- 状态：fixed（2026-08）

### UX-031 · 计划完成后粘条无法收起
- 来源：2026-08 浏览器自测聊天
- 场景：全部步骤已交付仍永久钉顶
- 不爽：AI 已下线还占一行
- 建议：完成态加「收起」，sessionStorage 记住
- 优先级：P2
- 状态：fixed（2026-08）

### UX-032 · 录像缩略图泄漏「操作员唤醒」注入文
- 来源：2026-08 浏览器自测 /replays
- 场景：队长录像卡片预览前几帧含 PTY wake 注入
- 不爽：小白看到内部指令像系统在自言自语
- 建议：castPreview 过滤唤醒/注入噪声行
- 优先级：P1
- 状态：fixed（2026-08）

### UX-033 · 已完成任务仍可「标记阻塞」
- 来源：2026-08 浏览器自测 /tasks
- 场景：完成列卡片同时露出「标记阻塞」+「归档」
- 不爽：已完成还能点阻塞，像状态机坏了
- 建议：done/archived 隐藏 block
- 优先级：P2
- 状态：fixed（2026-08）

### UX-034 · AI 离线仍挂「需要你·跟队长说」僵尸条
- 来源：2026-08 浏览器自测审计试用空间
- 场景：fixer/docs-writer handoff_missing（.error 已清），队长也已死；条仍写「跟队长说」
- 不爽：点聚焦空输入框；计划已完成还像欠债
- 建议：silent missing + 无 live orchestrator → 不进 NeedsYou；handoff_failed 仍亮
- 优先级：P0
- 状态：fixed（2026-08）

### UX-036 · 「研究委员会」+ 中文 UI 泄漏英文 judge / 安装说明
- 来源：2026-08 浏览器自测 fusion/consult/settings/plugins
- 场景：高级 Tab 写「研究委员会」；竞赛空态写 `judge 对比`；未装引擎卡片 summary 全英文
- 不爽：命名已走动作系，却夹洋气隐喻与英文术语；小白插件页看不懂
- 建议：多模对比；评审；install 文案 i18n；角色用 Git/IDE 中文（解冲突），禁「调和/会诊」类词
- 优先级：P1
- 状态：fixed（2026-08）

### UX-037 · 插件页/会诊页泄漏英文 binary、Panel、判决
- 来源：2026-08 浏览器自测 settings/plugins + consult
- 场景：插件卡片标 `binary`；多模对比写 Panel / 落判决
- 不爽：动作系中文之后仍夹开发黑话
- 建议：命令 / 模型 / 已选定；插件说明不提 toml/server
- 优先级：P1
- 状态：fixed（2026-08）

### UX-038 · 设置/MCP/台账泄漏 agent、spell、编排、台账
- 来源：2026-08 浏览器自测 /settings /mcp /ledger
- 场景：通用设置写 spell/agent；MCP 副标题写 MCP server；高级 Tab 写工作台账
- 不爽：装包用户不是财务也不是框架作者
- 建议：成员/同一批任务；外部工具；工作记录
- 优先级：P1
- 状态：fixed（2026-08）

### UX-039 · 任务看板泄漏 hospital「待分诊」和黑板 key
- 来源：2026-08 浏览器自测 /tasks /cron /dag
- 场景：完成列卡片显示 `claude-a` + `→ researcher.done`；列名「待分诊」；定时页写 5 段 cron；协作图空态写活跃 agent
- 不爽：医院分诊 + 内部黑板文件名，装包用户看不懂
- 建议：待处理；已交付/未交结果；运行时间预设；没有在线成员
- 优先级：P1
- 状态：fixed（2026-08）

### UX-040 · 用量「按 AGENT」+ 隐私页泄漏 shim / bypass / localStorage
- 来源：2026-08 浏览器自测 /usage /settings/privacy
- 场景：h2 uppercase 把「按 agent」变成 AGENT；副标题写 PTY；隐私页写 --dangerously-skip-permissions
- 不爽：像给开发者看的内部页
- 建议：按成员；估算≠订阅账单；直接执行/本机偏好，不提 shim
- 优先级：P1
- 状态：fixed（2026-08）

### UX-041 · 设置·模型/快捷键/关于 泄漏开发备注
- 来源：2026-08 浏览器自测 /settings/models /shortcuts /about
- 场景：标题「模型配给」；说明写 `--model` / `model_args` / `--effort`；快捷键写 hardcoded/cmdk；关于页列出 crate + `swarmx-shim` + loopback
- 不爽：装包用户不是在读仓库 README
- 建议：模型/思考强度用引擎口径；快捷键写清「不能自定义」；关于只留版本+仓库+本机服务地址
- 优先级：P1
- 状态：fixed（2026-08）

### UX-042 · 命令面板泄漏 agent / Wizard / 编排
- 来源：2026-08 浏览器自测 ⌘K
- 场景：占位「唤醒 agent」；快捷项写「打开 Wizard」；分组「唤醒 agent」
- 不爽：命令面板是高频入口，黑话比设置页更扎眼
- 建议：叫醒成员；打开向导
- 优先级：P1
- 状态：fixed（2026-08）

### UX-043 · 目标页占位符是仓库内部验收清单
- 来源：2026-08 浏览器自测 /goals
- 场景：副标题写防止 agent 跑偏；目标/验收占位写 SDK/API、交互式 PTY、Goal API；字段叫 Token 预算
- 不爽：空表单像给本仓库开发者填的，装包用户会以为自己也要改传输层
- 建议：副标题讲目标和做完标准；占位用登录页类通用例子；用量上限
- 优先级：P1
- 状态：fixed（2026-08）

### UX-044 · 录像库搜索/卡片泄漏 agent_id 与 .cast
- 来源：2026-08 浏览器自测 /chat/05f13a22/replays
- 场景：搜索框「按 agent / id」；卡片旁 8 位 id；播放无障碍名 `claude-a3f6e93f`；下载提示「.cast」
- 不爽：角色标签已经够认人，还甩内部 id 和文件格式
- 建议：按成员或角色搜索；卡片不显示短 id；下载录像
- 优先级：P1
- 状态：fixed（2026-08）

### UX-045 · 协作图全停确认仍写 agent
- 来源：2026-08 浏览 /chat/05f13a22/dag 时对照文案
- 场景：确认框「暂停所有 agent」；toast 写 n 个 agent
- 不爽：侧栏已改成成员，这里还用内部词
- 建议：暂停/恢复成员
- 优先级：P2
- 状态：fixed（2026-08）

### UX-046 · 发起竞赛表单 uppercase 把命令喊成 PYTHON3 CHECK.PY
- 来源：2026-08 浏览器自测 /chat/05f13a22/fusion 点发起竞赛
- 场景：标签带 `uppercase`，拉丁命令被显示成 PYTHON3 CHECK.PY / CARGO TEST；副文写「首次并行化」、CLI agent
- 不爽：和用量页「按 AGENT」同一类事故；装包用户被命令格式吓到
- 建议：去掉 uppercase；验收命令/git 提示用普通计算机中文，不写并行化/CLI agent
- 优先级：P1
- 状态：fixed（2026-08）

### UX-047 · 删除工作空间确认仍写 agent
- 来源：2026-08 浏览器自测侧栏「删除 审计试用空间」
- 场景：确认框正文「所有 agent 会一并停止」
- 不爽：破坏性操作还用内部词；删之前最后一眼是黑话
- 建议：还在跑的成员会一并停下
- 优先级：P1
- 状态：fixed（2026-08）

### UX-048 · 多模对比副标题泄漏 zulu / license / +2 次调用
- 来源：2026-08 浏览器自测 /chat/05f13a22/consult
- 场景：副标题「zulu 一把 license」；成本写「模型数 + 2」；占位「反方检查」
- 不爽：装包用户不认识 zulu，公式像给内部看的
- 建议：讲多个模型同时作答；按所选模型数计费；占位用选型/方案比较
- 优先级：P1
- 状态：fixed（2026-08）

### UX-049 · MCP 页 STDIO 大写 + uv 把 git hash 甩给用户
- 来源：2026-08 浏览器自测 /mcp
- 场景：卡片标签 `uppercase` 把 stdio 显示成 STDIO；uv 版本带 commit 和时间戳；Context7 写「注入 prompt」；说明写「重建」
- 不爽：像开发机探测输出，不是装包产品
- 建议：标签「本机」；版本只留数字；文档交给 AI；重新拉起
- 优先级：P1
- 状态：fixed（2026-08）

### UX-050 · MCP 开关太胖
- 来源：2026-08 用户截图 /mcp 卡片 Claude/Codex 开关
- 场景：sm 开关实际 40×32，圆钮 20×20，`min-h-8` 钉死和默认一样高
- 不爽：看起来像方块，比旁边的字还高一截
- 建议：sm 收到 28×16 / 12×12；默认开关一并收到 36×20
- 优先级：P2
- 状态：fixed（2026-08，待用户再看一眼）

---

1. 跑 TC 时顺手记：哪一步骂了脏话 → 就是候选。
2. 一条只写一个痛点；别把三个 bug 揉成一条。
3. 能复现就挂 TC 号；不能复现标 `[GUESS]` 并降优先级。
4. 修完把状态改 `fixed`，在 PR 里链回 UX-id。
