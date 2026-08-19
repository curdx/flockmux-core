# swarmx 全量批判性审查报告

生成时间：2026-06-13　方式：28 个独立审查员逐页/逐后端域元素级审查 + 每条对抗式复核（驳回 40 条误报）

## 总览

| 级别 | 数量 | 含义 |
|---|---|---|
| 🔴 P0 | 8 | 会让用户用不了或数据出错，必须尽快修 |
| 🟠 P1 | 40 | 明显影响体验，但有 workaround |
| 🟡 P2 | 130 | 打磨项 / 改进建议 |

### 按页面/域分布

| 页面/域 | P0 | P1 | P2 |
|---|---|---|---|
| 终端页 Terminal | 1 | 2 | 2 |
| 依赖图 DAG 视图 | 1 | 1 | 5 |
| 回放视图 + 播放器 + 录制面板 | 1 | 1 | 8 |
| 文件接口 | 1 | 1 | 2 |
| 账本 Ledger 视图 | 1 | 1 | 5 |
| 黑板/Swarm 面板 + 通用组件 | 1 | 1 | 5 |
| 任务页 Tasks | 1 | 0 | 5 |
| 服务启动与资源解析(main + 资源目录) | 1 | 0 | 2 |
| 对话主面板 MessagesPanel(核心) | 0 | 4 | 3 |
| Usage 接口与计价 | 0 | 3 | 0 |
| 目标页 Goals | 0 | 3 | 5 |
| 设置页 Settings | 0 | 3 | 3 |
| 首页/工作空间列表 + 新建空间向导 | 0 | 3 | 2 |
| MCP 管理页 | 0 | 2 | 3 |
| 前端 API 层 + 基址解析 | 0 | 2 | 4 |
| 定时任务页 Cron | 0 | 2 | 6 |
| 通知中心 + 通知弹窗 | 0 | 2 | 8 |
| Cron 接口 | 0 | 1 | 5 |
| MCP 接口 | 0 | 1 | 5 |
| Prompt 优化 / Recording 接口 | 0 | 1 | 2 |
| Tasks / Goals 接口 | 0 | 1 | 6 |
| 全局外壳/命令面板/模型选择/Spell启动/Agent抽屉 | 0 | 1 | 5 |
| 工作空间外壳 + 侧边栏 + 工具条 | 0 | 1 | 6 |
| 文件页 Files | 0 | 1 | 8 |
| 用量与成本页 Usage | 0 | 1 | 7 |
| 调试页 Debug | 0 | 1 | 5 |
| Blackboard 与消息接口 | 0 | 0 | 4 |
| Spell / Role / Plugin 接口 | 0 | 0 | 3 |
| WebSocket：pty / swarm / terminal | 0 | 0 | 6 |

---

## 🔴 P0（8）

#### P0-1 看板操作（标阻塞/完成/归档/复位）失败时被完全静默吞掉，UI 撒谎说成功
- **页面/域**：任务页 Tasks　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：在卡片上点「阻塞/完成/归档/复位」，确认后卡片立刻乐观地移动到目标列；如果后端写库失败（500、断网、agent_id 不存在），用户看不到任何错误提示，卡片要么停在错误的列、要么在 4 秒轮询后悄悄弹回原位，用户以为自己操作成功了。
- **复现**：1) 起后端，打开 /tasks，有至少一个 worker。2) 用 devtools 把 POST /api/tasks/:id/status 拦截成返回 500（或直接拔网）。3) 点某卡片「完成」并确认。4) 观察：卡片乐观跳到 Done 列，无任何报错；4 秒后轮询把它弹回原列，全程零提示。
- **根因**：web/src/routes/tasks.tsx:64-83（setStatus 无 catch，乐观更新后失败静默）；触发点 web/src/api/http.ts:80-98（非 2xx 抛 ApiError）；后端失败源 crates/swarmx-server/src/routes/tasks.rs:96-109（400/500）；可用而未用的诚实反馈设施 web/src/lib/toast.ts + web/src/App.tsx:130
- **影响**：操作者以为已把某个 worker 标记为 blocked/done（可能据此做了运维决策，比如不再去管它），实际后端没记下来，4 秒后状态又跳回去，造成误判和反复操作。是这个项目明令禁止的「状态撒谎」。
- **建议修法**：给 setStatus 加 catch：捕获后回滚乐观更新（保留改动前的快照）或至少弹一个错误 toast/内联错误条，并提示可重试。最简单：catch 里 setErr(true) 复用现有错误条，且不依赖 finally 的 load() 来「纠正」——因为纠正本身也是无声的。

#### P0-2 「暂停所有/恢复所有」按 workspace 全停，但 DAG 是 thread(direction) 范围，跨方向误停别的会话的 agent
- **页面/域**：依赖图 DAG 视图　**维度**：功能正确性 / 状态诚实性 / 越权范围　**置信度**：high
- **现象**：在某个 direction(thread)的依赖图里点「暂停所有 agent」，会把同一 workspace 下其它 direction 里正在跑的 agent 也一起 Ctrl-C 暂停；按钮旁的计数和确认弹窗里写的数字却只是当前 direction 的 live 数，用户以为只停了眼前这几个。
- **复现**：建一个有 ≥2 个 direction 的 workspace，每个 direction 各 spawn 1 个 worker 跑起来；进入 direction A 的 DAG 视图，点右上「暂停所有 agent」确认；切到 direction B，发现 B 的 worker 也变成已暂停。
- **根因**：web/src/routes/workspace/views/Dag.tsx:422-433 (onInterruptAll 调 workspace 级 interruptAllInWorkspace) + crates/swarmx-server/src/routes/rest.rs:1987-1992 (后端只按 workspace_id 过滤、不看 thread_id)；范围不自洽点：确认弹窗 count Dag.tsx:478-481 与文案 Dag.tsx:482-485，及 onResumeAll thread 级 Dag.tsx:438-457
- **影响**：多 direction 协作时，操作者在 A 方向点暂停，B 方向正在干活的 worker 被静默中断、自动唤醒被关掉，直到有人手动恢复——用户根本不知道是这一下点出来的。属于「界面撒谎 + 越权影响别的会话」，是项目红线。
- **建议修法**：两选一并保持自洽：(a) 真按 workspace 停——就把按钮文案/计数改成 workspace 级(用 activeMembers 而非 thread 过滤后的 liveAgents 计数)，并明确告诉用户会影响所有 direction；或 (b) 按 direction 停——给后端 interrupt-all 加 thread_id 查询参数(对齐 rest.rs:1990 的过滤)，前端传 activeThread.id，让 interrupt 与 resume 范围都收敛到当前 direction。当前 interrupt(workspace)与 resume(本地 thread 列表)范围不一致，务必统一。

#### P0-3 安装版里「下载 .cast」按钮是死的，且会把整个 webview 导航走、卡死 App
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：真实用户路径(安装版) / 功能正确性　**置信度**：high
- **现象**：用户在 Replays 卡片、全屏播放器头部、或 AgentDrawer 里点「下载」(Download 图标)，期望弹出保存对话框；实际什么都没下载，webview 直接跳转到 http://127.0.0.1:7777/api/recording/:id 显示一坨 JSON-lines 原文，且没有返回按钮，只能重启或手动改 URL 才能回到 App。
- **复现**：tauri build 出安装包 → 打开任意有录像的 workspace → Replays 标签 → 点卡片右下「下载」。观察 webview 被导航到原始 cast 文本、无法返回。
- **根因**：根因定位正确(Replays.tsx:291-299、replays/player.tsx:203-210、agent/AgentDrawer.tsx:633-644 + http.ts:286-287 + apiBase.ts:18-23 + recording.rs:93-98 均属实)。补充：还有一处同样的跨域 <a href download> 在 web/src/components/RecordingsPanel.tsx:119-128，修复时需一并处理。
- **影响**：安装版用户(项目头号准则关注的真实用户)完全无法保存录像，且每次点击都把 App 导航坏掉、需重启。开发机/纯 web 版同源所以察觉不到 —— 正是 CLAUDE.md 反复警告的「仓库里好好的、打成包就坏」类事故。
- **建议修法**：改成同源 blob 下载：fetch(api.recordingCastUrl(id)) → res.blob() → URL.createObjectURL → 临时 <a download>.click() → revokeObjectURL（复用 settings.tsx:964-974 模式，抽成 lib/download.ts），期间给按钮加 loading/disabled。或后端给 GET /api/recording/:id 加 Content-Disposition: attachment（但仍需确认 Tauri 跨域是否拦下载流；blob 方案最稳）。

#### P0-4 无 workspace_id 的裸调用 + MCP 子进程零鉴权 = 任意本地进程可读全盘文件（仅靠一份极窄的敏感名单兜底）
- **页面/域**：文件接口　**维度**：8. 安全/隐私（越权/任意文件读取）　**置信度**：high
- **现象**：任何在用户机上跑起来的本地进程（恶意 npm 依赖、被 prompt-injection 操纵的 agent 子进程、落地的命令执行）只要往 http://127.0.0.1:7777/api/files/read?path=/任意绝对路径 发一个不带 Origin 头的 GET，就能把该文件内容读回来；同样可用 /api/files/list 遍历整个磁盘。完全不需要知道任何 workspace_id、token 或密钥。
- **复现**：起服务后在另一终端跑：curl 'http://127.0.0.1:7777/api/files/read?path=/etc/hosts'（无 Origin 头）→ 返回文件内容；curl 'http://127.0.0.1:7777/api/files/read?path='$HOME'/.npmrc' → 返回 npm token（.npmrc 不在 is_sensitive 名单里）。
- **根因**：crates/swarmx-server/src/routes/files.rs:251 (read_file 裸调用跳过 jail) / files.rs:185 (list_dir 同) / files.rs:111 (is_sensitive 窄黑名单) / main.rs:872-895 (require_local_origin 放行无 Origin 请求)
- **影响**：这是该项目红线级越权：一个被注入的 agent 或一条恶意依赖即可把用户主目录里的源码、~/.config/* 下绝大多数配置、~/.docker/config.json、~/.npmrc、浏览器 cookie 库、~/.bash_history、私有仓库代码等悉数读走并外传。is_sensitive 只挡了 .ssh/.aws/.gnupg/.kube/.azure/.config/gcloud/.claude.json/.netrc/.pgpass/.git-credentials + *.pem/*.key/.env/含 credential 等极少数；其余高价值文件全裸奔。
- **建议修法**：裸调用（无 workspace_id）不应等于『全盘可读』。最小修复：去掉裸调用的无限制特例——要求所有 read/list 都必须落在某个 workspace 的 roots 内，all=1 仅作为显式 UI 开关且不可由无 Origin 的进程触发；或者对『无 Origin』的请求（即非浏览器、非用户本人点击）禁用 all=1 与裸全盘读，只允许 workspace-jail 内访问。根因上，loopback+无 token 的模型决定了『同机任意进程=可信』，但文件读接口把这个假设放大成了任意文件读取 oracle，需要把文件接口收敛到 workspace 白名单而不是黑名单。

#### P0-5 Tauri 丢弃 sidecar 的事件接收端(_rx)：后端启动失败时，安装版用户看到一个连不上后端、毫无报错的死界面
- **页面/域**：服务启动与资源解析(main + 资源目录)　**维度**：状态诚实性 / 错误处理与失败恢复 / 真实用户路径　**置信度**：high
- **现象**：release 安装版里 swarmx-server 作为 sidecar 启动。如果它在启动早期失败（例如端口 7777 被上次崩溃残留的孤儿进程占着→main 里 acquire_singleton_lock 走 std::process::exit(2)；或 Store::open/create_dir_all 因 HOME 只读而 ? 返回 Err 退出），Tauri 端只会 log::error! 到日志文件，UI 窗口照常打开。用户面对的是一个永远连不上 127.0.0.1:7777 的界面，没有任何「后端没起来」的提示，也无从自助恢复（只能重装/手动清进程）。
- **复现**：1) 在 release 安装版运行后强杀 Tauri 但留下 swarmx-server 孤儿占着 7777（或先手动起一个占 7777 的进程）；2) 再次打开 app；3) 新 sidecar 因 singleton-lock 冲突 exit(2)；4) 观察：窗口正常打开，但所有 /api 请求失败，界面无任何「后端未启动」提示。
- **根因**：web/src-tauri/src/lib.rs:84 (primary: `_rx` discarded) + web/src/routes/chat/Home.tsx:67-75 (co-cause: backend-down fetch failure swallowed → renders the clean "create your first workspace" Welcome splash, affirmatively misleading)
- **影响**：首次/重启打开应用，若端口被占或数据目录不可写，用户得到一个完全不工作且不解释为什么的应用——违反「下载→安装→打开→立刻能用」与状态诚实性红线。这正是项目最高准则反复强调的那类「开发机好好的、安装版坏掉」事故的运行期版本。
- **建议修法**：在 lib.rs 的 release 分支里 take 住 rx 并 tokio/async 监听：收到 CommandEvent::Terminated 或 Error 时，emit 一个 Tauri 事件（或 set 一个 app state 标志）让前端弹出「后端启动失败：<stderr 尾部>」并提供重试/查看日志按钮；同时把 stderr 落到日志。最低限度也要在收到 Terminated 时把 stderr 末几行写日志，而不是整段静默。

#### P0-6 后端挂了/拉取工作区失败时,终端页吞掉 error 并撒谎说「连接成功/为全部工作空间打开」
- **页面/域**：终端页 Terminal　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：后端没起或网络断时,useToolWorkspaces 的 listWorkspaces() 失败,ready 仍翻 true。终端页看到的是:工作区下拉被隐藏(workspaces.length===0),确认卡片标题正常显示、描述写「将为『全部』打开一个真实 shell」,「连接终端」按钮从 loading 变为可点击且 enabled。用户点击后 WebSocket 静默连接失败,xterm 只是一片漆黑,没有任何报错。
- **复现**：1) 关掉 swarmx-server(后端 7777 不起);2) 打开 /terminal;3) 观察:下拉消失,卡片说『将为全部打开 shell』,『连接终端』按钮可点;4) 点击连接 → 黑屏,无任何错误提示。
- **根因**：web/src/routes/terminal.tsx:48
- **影响**：这是项目红线『状态诚实性』的正面违反:界面在后端不可用时显示『可连接/为全部工作空间打开』,用户点了连接却得到一个黑屏终端,完全不知道是后端没起还是自己操作错了,只能凭经验去重启。对小白用户尤其致命——既不告知失败,也无自助恢复路径。
- **建议修法**：在 terminal.tsx:48 解构出 error,并在渲染时优先分支:若 error 非空,展示『加载工作区失败/后端未连接』的错误态卡片 + 重试按钮,而不是展示可点击的连接卡片;同时 disabled 条件应为 disabled={!ready || !!error}。确认卡片的 workspace 文案在 error 态下不应回退成『全部』。

#### P0-7 「压缩」按钮在默认安装版上 100% 失败，却谎报「已是最简，无需压缩」
- **页面/域**：账本 Ledger 视图　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户点顶栏「压缩」，转一下圈，然后看到提示「已是最简，无需压缩」。实际上后端根本没压缩——它返回了 402 错误，前端把错误吞掉伪装成「无需压缩」。
- **复现**：默认安装版（未设 SWARMX_ALLOW_CLAUDE_PRINT）打开任意空间 → Ledger 标签 → 点「压缩」。无论台账多大都显示「已是最简，无需压缩」。Network 面板可见 POST /api/blackboard/compact 实际返回 402。
- **根因**：web/src/routes/workspace/views/Ledger.tsx:191-203
- **影响**：这是项目红线「界面不许撒谎」的直接违反：一个功能 100% 不可用，UI 却显示成功态（且是'无需操作'这种最具误导性的成功）。用户永远不知道真实原因，也拿不到后端给的自助开启指引。同理 503（无 claude 插件）、502/504（claude 失败/超时）、500 也全被吞成「无需压缩」。
- **建议修法**：不要 `.catch(() => null)`。捕获 ApiError 后区分：成功(changed)→显示省了多少；402/503→把后端 detail（含开启引导）显示为说明而非'成功'；其它错误→红色错误态。即把 compactNote 拆成 ok/info/error 三态，错误用 state-danger 文案展示 `(e as ApiError).detail`。

#### P0-8 「新建文件」对已存在路径会用空内容静默覆盖，造成不可见的数据丢失
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：数据完整性 / 状态诚实性　**置信度**：high
- **现象**：在左上角输入框敲一个已存在的文件名（例如 design.md、task.ledger.md）再点 +，该黑板文件被直接清空成空白，UI 没有任何警告，左侧列表里看起来还在、点开却是空的。
- **复现**：1) 让 swarm 写出 design.md（有内容）。2) 在共享区左上输入框敲 `design.md`，点 + 或按回车。3) design.md 内容被清空，无任何提示，且历史里只剩一条新的空 write。
- **根因**：BlackboardPanel.tsx:134-146 createNew() 无条件 `api.writeBlackboard(path, { content: "" })`，没有先检查 path 是否已存在（entries 里能查到却不查）。后端 write_blackboard（crates/swarmx-swarm/src/swarm.rs:514）是纯 upsert/覆盖语义，也不区分『新建』和『改写』，所以前端这一下就把已有内容覆盖为空。
- **影响**：黑板是整个 swarm 的协作记忆（design.md、各 role 的 handoff 信号都在这里）。操作者想『新建』一个文件却撞名，等于一键抹掉 agent 正在依赖的关键文件，触发的是空内容写入而非删除——还会广播 blackboard_changed(op=write)，下游 depends_on 订阅者被错误唤醒去读一份空文件。属于跨 agent 的隐性数据损坏。
- **建议修法**：createNew 里先判重：若 entries 已含该 path（或先 readBlackboard 成功），不要覆盖——改为直接 openPath(path) 选中它，或弹 ConfirmActionDialog（variant=destructive）确认『该文件已存在，确定要清空重建吗』。理想是后端补一个 create-only（已存在则 409）语义，前端据此提示。


---

## 🟠 P1（40）

#### P1-1 run_cron 把真正的内部错误（DB 故障/复活失败）伪装成 HTTP 200 的 "skipped"，前端无法区分「无编排器跳过」和「系统真的炸了」
- **页面/域**：Cron 接口　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户点「Run now」，遇到数据库 busy、send_message 失败、init spell 没生成编排器、甚至 workspace 已删除等任何错误时，接口都返回 200 OK + {ok:false, skipped:"<原始错误串>"}。前端 cron.tsx:352 只判断 `!r.ok && r.skipped` 就当成一条普通红字提示，用户看到一段看不懂的 Rust 错误串（如 `database is locked` / `workspace not found`），既不知道是临时问题还是永久坏掉，也不知道该重试还是该删任务。
- **复现**：1) 让任意 cron job 指向一个已软删除的 workspace（删空间后该 job 变 orphaned）；2) 在 /cron 点该任务的「Run now」；3) 后端 revive_orchestrator 走 get_workspace_by_id... 注意 get_workspace_by_id 不过滤 deleted_at，若 workspace 行还在则继续，否则返回 None→Err("workspace not found")；4) 观察响应是 200 而非 4xx/5xx，且 UI 直接显示原始错误串。
- **根因**：crates/swarmx-server/src/routes/cron.rs:240-248 (run_cron collapses every run_job Err into 200 + {ok:false, skipped}); error sources undifferentiated in crates/swarmx-server/src/cron.rs:217-245 (run_job) and 251-276 (revive_orchestrator)
- **影响**：状态撒谎红线：失败被静默降级成 200，监控/前端都以为请求成功了。用户遇到瞬时 DB busy 本可重试，却被一段技术错误串劝退；遇到 orphaned 任务也只看到 `workspace not found` 而非「该空间已删除，请删除此任务」。无法自助恢复。
- **建议修法**：在 run_job 区分两类结果：返回 `Result<RunOutcome,String>`，其中 RunOutcome::Skipped(reason) 才走 200+{ok:false,skipped}，真正的 Err（DB/spawn 故障）走 500 + {error}。run_cron 据此分流；前端对 500 给「服务出错，可重试」、对 skipped 给业务文案。至少把 revive 失败、DB 错误与「无编排器且无法复活」用不同 message 前缀区分。

#### P1-2 改密钥/重装时先删后加，add 失败后旧配置已永久丢失且无回滚——把工作中的 MCP 改没了还显示报错
- **页面/域**：MCP 接口　**维度**：数据完整性 / 状态诚实性 / 错误恢复　**置信度**：high
- **现象**：用户对一个已正常工作的 server（如 context7）点「改密钥」或重新启用，若 add 阶段因任何原因失败（key 临时输错、网络抖动、claude/codex 进程被杀、CLI 升级导致语法变化、磁盘满），界面弹出错误，但用户原本可用的那条 MCP 配置已经被删掉、不会恢复。原本只是想改个 key，结果 server 直接没了。
- **复现**：1) 启用 context7（claude）并填一把可用 key，确认 status 里有 context7。2) 点「改密钥」，输入一把会让 `claude mcp add` 失败的值，或在 add 执行瞬间让 claude 不可用（如临时改 PATH/重命名 claude）。3) 观察返回 500/502 错误。4) 重新 GET /api/mcp/status：context7 已从 claude 配置消失，而用户只是想改 key。
- **根因**：mcp_admin.rs:420-438 的 upsert 逻辑：`let _ = run(bin, &remove_args).await;`（无条件先 remove 并忽略其结果），紧接 `let res = run(bin, &a).await;`（再 add）。remove 与 add 是两次独立子进程调用，中间没有任何快照/回滚；add 失败时直接返回 Err，删掉的旧条目不再恢复。这是经典的非原子 upsert 数据丢失窗口。
- **影响**：用户主动操作（改 key/重装）反而摧毁原本可用的配置，且对所有工作区、正在运行的 agent 立即生效——依赖该 MCP 的 agent 当场失能。属于「操作越想修越坏」，且失败提示不会告诉用户「你原来的配置已经没了」，状态展示不诚实。
- **建议修法**：改成「add 成功才算数」：先把当前条目内容读出来留底（或先 add 到临时名/校验新条目可加再切换），add 成功后再 remove 旧的；或捕获 remove 的成功结果，add 失败时用留底数据 re-add 回滚。最低限度：add 失败的错误信息里明确告知「原配置已被移除，请重试」，不要让用户以为只是没改成功。

#### P1-3 安装成功后若 status 复查失败，整页回退到「加载中…」并把已成功的开关显示成未装（状态撒谎）
- **页面/域**：MCP 管理页　**维度**：状态诚实性　**置信度**：high
- **现象**：用户拨开 Claude 开关，后端 `claude mcp add` 真成功了，但紧接着 reload 里的 `api.mcpStatus()` 偶发失败时，status 被置为 null。结果：该卡片乃至所有卡片的开关全部变回未勾选，右侧重新转「加载中…」，看起来像「没装成」。实际配置已经写进了 ~/.claude.json。
- **复现**：1) 打开 /mcp 2) 拨开 Claude 的 chrome-devtools 3) 在 install 返回后、reload 的 status 请求恰好失败（断网/后端瞬断/500）→ 卡片全部回到未勾选 + 「加载中…」，但配置实际已写入。
- **根因**：McpPanel.tsx:75-82 (reload 吞错置 null) + McpPanel.tsx:94-108 (runOp 先安装后 reload、reload 失败不进 catch、error 不置位) + McpPanel.tsx:188-189 (status=null→开关全灭) + McpPanel.tsx:299 (!status→加载中) + 附:McpPanel.tsx:283/290 (status=null 时开关 disabled，页内无法自救，需整页刷新)
- **影响**：项目红线（界面不许撒谎）的正面违反：真成功被显示成未完成/未装，用户会重复点击，可能反复 add/remove 真实用户配置，并对正在运行的 agent 造成抖动。也违反「失败不能默默吞」。
- **建议修法**：reload 拆成两类：成功安装后只在新数据可用时才覆盖，status 拉取失败时不要清空旧 status（保留乐观/旧值或单独置 reloadError 并提示「已写入，但刷新状态失败，点此重试」），并把刷新失败显式暴露成 error 而非静默 null。runOp 里 op 成功后即使 reload 失败也应提示「操作已生效，状态刷新失败」。

#### P1-4 初次加载失败时整页无错误提示、无重试，永久停在「加载中…」（卡死页）
- **页面/域**：MCP 管理页　**维度**：空/加载/错误/边界态　**置信度**：high
- **现象**：首次进入 /mcp 时若后端没起或断网，env 与 status 都 catch 成 null：runtime chip 永久转圈，每张卡片右侧永久「加载中…」，没有任何错误横幅，也没有「重试」按钮。用户只能猜是不是坏了或一直等。
- **复现**：关掉后端（端口 7777 不监听），打开 /mcp → 永久 spinner，无错误、无重试。
- **根因**：McpPanel.tsx:75-85 reload 把 env/status 失败都吞成 null 且不写 error；McpPanel.tsx:299-304 当 `!status` 时只渲染「加载中…」spinner，没有「加载失败」分支或重试入口。组件级 error 状态只在 runOp（安装/卸载）路径里被设置，初次 load 路径根本不会触发它。
- **影响**：符合红线「不许一直转圈不给结果」。小白用户面对一个永远转圈、没有任何文字解释也没有恢复手段的页面，只能重启应用。
- **建议修法**：reload 捕获失败时设置 loadError 并渲染「加载失败 — 重试」按钮（手动重新 reload）。区分 loading（首次）与 loaded-empty/loaded-error 三态。

#### P1-5 claude -p 退出码 0 但 stdout 实为错误/拒绝文本时，会被当成「优化结果」塞进 composer
- **页面/域**：Prompt 优化 / Recording 接口　**维度**：状态诚实性 / 错误处理　**置信度**：medium
- **现象**：用户点「优化」，草稿被替换成一段并非优化结果的文本——可能是 claude 的拒答（"I cannot help with that..."）、登录/额度提示，或某些版本下 exit 0 仍打印的提示语。界面表现为「优化成功」，实际把垃圾内容写进了草稿。
- **复现**：在未登录或额度耗尽（但 claude 仍 exit 0 打印提示）的环境，设 SWARMX_ALLOW_CLAUDE_PRINT=1，对一段 >=8 字符的草稿点「优化」，观察 composer 被替换为提示文本而非真正的改写。
- **根因**：crates/swarmx-server/src/routes/rest.rs:2589（仅 out.status.success() 判成败）+ 2604-2618（exit 0 即无条件信任 stdout，无 is_error/内容校验）
- **影响**：触碰状态诚实性红线：把「未验证的模型输出」当成「已确认的优化结果」展示。最坏情况用户没注意直接发送了被污染的草稿。前端有 undo（MessagesPanel.tsx:738）能兜底，但前提是用户察觉到不对。
- **建议修法**：不要无条件信任 stdout：(1) 改用 `--output-format json` 并解析 `is_error`/`subtype` 字段，仅在确为正常 result 时采纳；或 (2) 至少加启发式护栏——若 cleaned 与原文长度比异常（如远短于原文、或包含明显的拒答/登录关键词），降级为「未改动 + 提示」而非替换。当前 text 模式 + 仅看退出码无法区分「真改写」与「错误文本」。

#### P1-6 list_tasks 用 unwrap_or_default() 把 DB 错误吞成空列表，显示"暂无任务"而非报错
- **页面/域**：Tasks / Goals 接口　**维度**：状态诚实性/错误处理　**置信度**：high
- **现象**：当 store.list_tasks 因 DB 锁/损坏/IO 失败时，handler 把错误吞掉返回 200 {"tasks":[]}。前端 tasks.tsx 的 err 标志永远不会被触发，Kanban 显示空状态文案 No tasks yet，用户以为没有任务，实际是后端读库挂了。
- **复现**：在 list_tasks SQL 涉及的表上制造一个临时错误（如让 blackboard_ops 不可读触发 query 失败），观察接口仍回 200 空数组。
- **根因**：crates/swarmx-server/src/routes/tasks.rs:71 let rows = state.store.list_tasks(ws).await.unwrap_or_default(); — Err 被静默降级为空 Vec。对比同域 goals.rs:90-100 list_goals 正确地把 Err 映射成 500 {"error":...}。
- **影响**：数据库异常被伪装成正常但没数据，违反状态诚实性红线，且让运维/用户无法区分真没任务和后端坏了，排障困难。
- **建议修法**：改成 match：Ok(rows)=>Json(...)；Err(e)=>(500, Json({"error":e.to_string()}))，与 list_goals 一致。前端区分 5xx 显示 loadError。

#### P1-7 DB 查询出错被 unwrap_or_default() 静默吞掉，前端把「数据库失败」渲染成「你还没有用量」
- **页面/域**：Usage 接口与计价　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：当 agent_usage 查询真的报错（SQLITE_BUSY 重试耗尽、库损坏、JOIN 时 schema 异常等），用户看到的不是错误提示，而是一片祥和的「暂无用量数据」空状态，误以为自己没花钱/没跑过任务。
- **复现**：在 store 层让 usage_by_model 返回 Err（例如手动制造 SQLITE_BUSY 或临时改表名），打开 Usage 页 → 看到「暂无用量」而非错误。
- **根因**：crates/swarmx-server/src/routes/usage.rs:333-335
- **影响**：命中项目红线「不许把失败默默吞掉显示成功」。用户排查成本/排查为何用量不涨时会被彻底误导，且无任何自助恢复线索（连日志都只是 unwrap_or_default 后无声丢弃，error 没进 tracing）。
- **建议修法**：handler 改为把三处 Result 的 Err 向上传播成 5xx（或返回一个带 `error` 字段的 JSON 让前端区分），至少在 unwrap 前 `tracing::error!` 记录。前端在拿到 5xx 时已有 err 横幅，只需后端不再把错误压平成 200+空。

#### P1-8 usage_by_day 取的是「最早的 N 天」而非「最近 N 天」，长期用户的用量趋势图会停留在过去
- **页面/域**：Usage 接口与计价　**维度**：功能正确性　**置信度**：high
- **现象**：用量趋势折线图（UsageTrendChart）在累计使用超过 90 个不同日期后，显示的是最早 90 天的数据，最近的日期被悄悄丢弃——图表看起来「定格」在很久以前，且总量卡片与图表对不上。
- **复现**：构造 >90 个不同日期的 agent_usage 行（at 跨 100+ 天），调 /api/usage → by_day 返回的是最早 90 天，最近的日期缺失。
- **根因**：crates/swarmx-storage/src/store.rs:773-795 `usage_by_day` 的 SQL：`... GROUP BY day ORDER BY day ASC LIMIT ?2`（ws 分支 LIMIT ?2，无 ws 分支 LIMIT ?1）。函数文档/注释（store.rs:751、usage.rs 调用处传 90）声称是「last `days` days / 最近 days 天」，但 `ORDER BY day ASC` + `LIMIT` 取的是升序排在前面的、也就是最旧的 N 天，且 WHERE 里没有任何 `at >= now-Nd` 的时间下界。
- **影响**：趋势图对老用户不诚实地展示陈旧数据并隐藏近期活动（命中诚实性红线的「把还没验证/错误的状态当确认状态展示」）。totals 是全量聚合、by_day 是错的子集，二者口径不一致会让用户困惑。
- **建议修法**：改成先取最近 N 天：`... GROUP BY day ORDER BY day DESC LIMIT ?N`（或加 `WHERE at >= (strftime('%s','now')-? )*1000` 时间下界），并在返回前/前端再按 day ASC 排序绘图。同时修正 ws 分支与无 ws 分支保持一致。

#### P1-9 pricing 配置路径只认 HOME 环境变量，Windows 安装版会落到 CWD 相对路径，读写/重置全失效
- **页面/域**：Usage 接口与计价　**维度**：安装版现实（项目最高准则）/ 数据完整性　**置信度**：medium
- **现象**：Windows 用户在 Usage 页编辑并保存价格表（PUT /api/usage/pricing）后，要么写入失败（往安装目录/盘根写被拒），要么写到一个用户找不到的幽灵位置；重启后自定义价格不生效，「重置」也作用在错误路径上——表现为「保存后看着成功，下次打开又变回默认」。
- **复现**：在 Windows（不设 HOME）跑安装版 sidecar，CWD=安装目录或 /，PUT /api/usage/pricing → 写入相对 .swarmx/pricing.json，重启后 load_pricing_rules 读不到、回落默认。
- **根因**：crates/swarmx-server/src/routes/usage.rs:168-173
- **影响**：Windows 安装版的价格自定义/重置功能整体不可用且不诚实（PUT 返回里 path 字段会显示一个相对路径，用户照着去找文件根本不存在）。属于「装完包就坏」的典型。
- **建议修法**：统一改用跨平台 home 解析（dirs::home_dir() / 或 HOME 失败时回退 USERPROFILE），或由 Tauri 启动 sidecar 时把 `SWARMX_*` 绝对路径环境变量传进来并优先读取；usage.rs 至少先 USERPROFILE 兜底。这是仓库级修复，建议抽一个共用 home_dir() 帮手替换全部 var("HOME")。

#### P1-10 选中节点的「唤醒/暂停/恢复」按钮：选中已死 agent 时是死按钮，唤醒失败被静默吞掉
- **页面/域**：依赖图 DAG 视图　**维度**：状态诚实性 / 死按钮 / 错误处理　**置信度**：high
- **现象**：selectedId 持久化在 URL；当被选中的 agent 在你打开详情时已 killed/exited，右侧详情面板状态正确显示「已终止」，但底部「打开会话/唤醒/暂停」按钮仍可点；点唤醒后什么都不发生(后端 404)、界面无任何错误提示。
- **复现**：—
- **根因**：web/src/routes/workspace/views/Dag.tsx:519（空 catch 吞错）+ Dag.tsx:800-827（wake/pause 按钮对 killed/shim_exit 不 disabled）；selected 取自含死 agent 的集合见 Dag.tsx:406-408（agents 定义 370-376），后端 404 见 rest.rs:1125-1130（wake）与 rest.rs:1853-1857（interrupt）。根因定位准确，无需修正。
- **影响**：对已死 agent，唤醒/暂停是彻底的死按钮(点了无效、无反馈)；这正是项目红线「一直点没反应/把失败默默吞掉」。即便对活 agent，wake 的空 catch 也意味着任何唤醒失败都不会让用户知道。
- **建议修法**：(1) selected 为已 killed/shim_exit 时，禁用唤醒/暂停按钮(或整组只保留「打开会话」做复盘)；(2) 去掉 requestWakeSelected 里的空 `.catch(()=>{})`，失败时 setError 给提示、成功后 refresh，与 onTogglePauseSelected 的处理(Dag.tsx:459-474)保持一致。

#### P1-11 命令面板「唤醒 agent」失败被静默吞掉，用户以为已唤醒(撒谎)
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：在 ⌘K 里选某个 agent → 确认「唤醒」后，如果后端请求失败(agent 刚死、shim 已退、服务 500/断网)，界面没有任何提示——没有 toast、没有报错、确认框正常关闭。用户得到的反馈和成功完全一样，会理所当然地认为「已经唤醒了」。
- **复现**：1) ⌘K 打开命令面板，确保列表里有一个 live agent；2) 在另一终端 kill 掉该 agent 的进程或停掉后端；3) 点该 agent 的「唤醒」并确认；4) 观察:无任何错误提示，确认框照常关闭，看不出失败。
- **根因**：主根因 web/src/components/CommandPalette.tsx:308 定位正确。第二处路径有误: 实际为 web/src/components/agent/AgentDrawer.tsx:237 (审查员写成 AgentDrawer.tsx:237，漏了 agent/ 子目录)。此外审查员漏数了两处同模式同 bug: web/src/routes/workspace/views/Dag.tsx:519 与 web/src/routes/workspace/views/Chat.tsx:608——三者都是用户主动点「唤醒」、都走同一个无条件关闭的 ConfirmActionDialog，应一并修。真正的失败放大点在 web/src/components/ConfirmActionDialog.tsx:42-46(无条件 onOpenChange(false) + void run?.() 即忘)。注: MessagesPanel.tsx:655/697 与 Chat.tsx:745 是 sendMessage 成功后的 best-effort 二次 wake、其主操作错误已由 setError 暴露，可不改。
- **影响**：踩中项目红线「不许把失败默默吞掉显示成功」。唤醒是用户在 agent 疑似卡住时的主要自救手段；静默失败会让用户对着一个其实没被唤醒的死 agent 干等，无从判断是「唤醒没生效」还是「agent 本来就慢」，只能重启应用。
- **建议修法**：把空 catch 换成 toast.promise 包裹真实操作：`toast.promise(api.wakeAgent(id), { loading: '唤醒中…', success: '已发送唤醒', error: e => `唤醒失败:${e.message}` })`。两处(CommandPalette、AgentDrawer)都改。

#### P1-12 网络层失败（后端 sidecar 未起/挂掉）抛出的是裸 TypeError，最终把 "Failed to fetch"/"Load failed" 这种英文直接甩给小白用户
- **页面/域**：前端 API 层 + 基址解析　**维度**：错误处理与失败恢复 / 真实用户路径 / 一致性　**置信度**：high
- **现象**：当 swarmx-server sidecar 还没启动、崩了、或端口被占时，任何 api.* 调用的 fetch 会 reject 一个 TypeError（Chrome 文案 "Failed to fetch"，WKWebView/Safari 文案 "Load failed"）。这个不是 ApiError，所以所有 `e instanceof ApiError ? e.detail : (e as Error).message` 的调用方（files.tsx:83/120、WorkspaceSidebar.tsx:1114/1140、CreateWizard.tsx:242/398 等）都落到 else 分支，把这串英文原文当错误提示展示。
- **复现**：不启动 swarmx-server（或 kill 掉 7777 端口），打开前端，进入 /files 选一个工作区或点任意拉取按钮 → 错误区显示 "Failed to fetch" / "Load failed"。
- **根因**：web/src/api/http.ts:74（根因定位正确；关键是此行裸 `await fetch` 无 try/catch，连接层 reject 的 TypeError 绕过了 line 80 的 `!res.ok` → ApiError 归一化，最终以 (e as Error).message 形式在 files.tsx:83/120、WorkspaceSidebar.tsx:1114/1140、CreateWizard.tsx:242/250 等调用方暴露给用户。冷启动窗口由 web/src-tauri/src/lib.rs:82-97 即发即忘式 spawn 且无就绪门控所致）
- **影响**：安装版用户第一次打开、或服务崩溃重启的几秒内点任何按钮，看到的是一串看不懂的英文报错，无法自助判断「是不是该等一下/重启」。违反项目「下载→打开→立刻能用」与状态诚实性红线（把底层连接失败暴露成技术黑话）。
- **建议修法**：在 request() 里用 try/catch 包住 fetch；catch 到非 AbortError 的异常时，抛出一个归一化的 ApiError（建议 status=0, detail=可 i18n 的「无法连接到本地服务」key）。这样所有 `instanceof ApiError` 的调用方就能统一识别「服务没起」这一类，给出「正在启动/请重启」的引导，而不是甩英文。

#### P1-13 大量 mutation 调用用 `.catch(() => {})` 静默吞错（尤以 wakeAgent「唤醒」最危险），用户以为操作生效、其实失败且毫无提示
- **页面/域**：前端 API 层 + 基址解析　**维度**：状态诚实性（红线）/ 错误处理　**置信度**：high
- **现象**：点「唤醒 agent」(⚡) 等操作时，若后端返回非 2xx（agent 已死、workspace 不匹配、被 quiet-gate 挡），前端把 reject 直接 `.catch(() => {})` 丢弃，UI 不报任何错，看起来像「已唤醒」，但实际什么都没发生。
- **复现**：对一个已经退出的 agent 在命令面板点「唤醒」→ 后端 4xx/5xx → 界面无任何反馈，agent 仍不动。
- **根因**：调用点把 api 层抛出的 ApiError 直接吞掉：web/src/components/CommandPalette.tsx:308 `api.wakeAgent(...).catch(() => {})`、web/src/components/agent/AgentDrawer.tsx:237、web/src/routes/workspace/views/Dag.tsx:519、web/src/routes/workspace/views/Chat.tsx:608/745、MessagesPanel.tsx:655/697 等，多处同样模式。api 层（http.ts）本身设计上把错误 throw 出来是对的，但这些消费方选择了静默。
- **影响**：直接踩项目状态诚实性红线：失败被默默吞掉、UI 不显示失败、用户得不到任何「这次唤醒没成功，请重试」的信号，只能干等 agent 永远不动。属于「显示成功但底层没成」。
- **建议修法**：至少给 wakeAgent/interrupt/resume/kill 这类有副作用的 mutation 的 catch 里接 sonner toast（项目已依赖 sonner）给出失败提示，区分「agent 已不在/被挡/网络失败」。可在 api 层之上加一个薄 `withToastOnError()` 包装统一处理，避免每个调用点各写各的。

#### P1-14 castPreview.ts 整个模块是死代码，卡片缩略图永远是占位图标而非真实预览
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：功能正确性 / 一致性　**置信度**：high
- **现象**：Replays 卡片缩略区永远只显示一个静态 ▶ 图标 + cols×rows + 时长，从不显示录像首帧/首几行实际终端内容。用户无法在不点进去的情况下区分哪条录像是哪条。
- **复现**：grep -rn 'loadCastPreview|getCachedCastPreview|castPreview' web/src 只命中定义文件自身，无消费者；打开 Replays 看任意卡片缩略图均为静态图标。
- **根因**：web/src/routes/workspace/views/Replays.tsx:311-346（CastThumb 自渲染静态占位、未接线）；孤儿模块 web/src/lib/castPreview.ts:95,99；接线在 "workspace-as-first-class" 重构中从已删除的 web/src/routes/replays/index.tsx（原提交 1f80c20 中 line 24/282-289 有 wiring）丢失。后端省流假设失效点 crates/swarmx-server/src/routes/recording.rs:79
- **影响**：功能缺失（缩略图名不副实），同时携带一段有维护成本、有潜在副作用(fetch+abort)的死代码。castPreview 的实现还依赖『服务端不支持 Range 所以 abort 中断连接省流』的假设，而 recording.rs:79 用 tokio::fs::read 一次性把整个文件读进内存再整体返回，前端 abort 省不下后端的内存/IO —— 即使接上也达不到注释承诺的省流效果。
- **建议修法**：二选一：(a) 把 CastThumb 接上 loadCastPreview/getCachedCastPreview，真正渲染首帧文本（注意 useEffect 清理、卸载 guard、错误兜底）；(b) 既然不用就删掉 castPreview.ts，避免误导后人。若选 (a)，建议后端配合支持 HTTP Range 或单独提供 /preview 端点，否则每张卡片仍会触发整文件读取。

#### P1-15 开关(启用/停用)失败时静默吞错 + 未处理的 Promise 拒绝，开关无声回弹
- **页面/域**：定时任务页 Cron　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户点切换开关，后端 500 或断网时，开关乐观翻转后被 load() 悄悄翻回原状，界面没有任何错误提示——用户以为自己没点中，反复点；控制台抛 unhandled rejection。
- **复现**：停掉后端或让 /api/cron/:id PATCH 返回 500，在页面点任一任务的启用/停用圆点，观察开关瞬间翻转又翻回、无任何报错文案，浏览器控制台出现 unhandled rejection。
- **根因**：web/src/routes/cron.tsx:337-345
- **影响**：停用一个定时任务失败时用户毫不知情，以为已停用但其实仍在按点触发（或反之），属于项目红线‘界面撒谎’：展示的开关状态与后端真实状态不一致且无提示。
- **建议修法**：给 toggle 包 catch，失败时 setErr/setNotice 给出可读提示，并保证 load() 重新拉回真实状态；切换期间给开关加 disabled/in-flight 防重复点击。

#### P1-16 删除失败时静默吞错 + 未处理拒绝，已删行又凭空冒回来且无解释
- **页面/域**：定时任务页 Cron　**维度**：状态诚实性 / 错误处理与失败恢复 / 数据完整性　**置信度**：high
- **现象**：用户确认删除后，行立即消失（乐观），若后端删除失败，load() 又把这行拉回列表，用户看到‘删了又回来’，没有任何错误说明。
- **复现**：让 DELETE /api/cron/:id 返回 500，点确认删除，行先消失再被 load() 拉回，无任何报错文案，控制台 unhandled rejection。
- **根因**：web/src/routes/cron.tsx:362-367 (remove 无 try/catch + 调用点 cron.tsx:522 不 .catch；失败时 line 366 load() 因抛错被跳过，行静默消失而非经 load() 拉回，且全程无 setErr)
- **影响**：删除失败被吞，用户对‘到底删没删’产生困惑，且无法自助判断；与 toggle 同属红线的失败静默。
- **建议修法**：remove() 加 try/catch：失败 setErr 可读提示并保留/恢复该行；成功后再 load()。考虑把乐观删除挪到响应成功后或失败时显式回滚。

#### P1-17 切换工作空间后消息列表不刷新，旧/远程房间消息缺失，可能显示空房或串房间历史
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：数据完整性/功能正确性　**置信度**：high
- **现象**：用户从工作空间 A 切到工作空间 B：B 房间里只显示「最近全局 200 条」里恰好属于 B 的消息。如果 B 的对话比全局最近 200 条都旧，B 房间会显示空（或只剩零星几条），用户以为历史丢了；反之刚在 A 发的消息切回 B 再切回 A 时只靠 live 事件补，断连/丢事件就缺。
- **复现**：建一个有较多历史消息的工作空间 A，再建工作空间 B（让 A 的消息把全局最近 200 条占满）。打开 A→切到 B→切回 A：观察 A 是否还能看到首屏之外/更早的历史；在 A 发消息后切到 B 再切回 A，断开 /ws/swarm 观察是否缺消息。
- **根因**：crates/swarmx-storage/src/store.rs:1703-1717 (+ web/src/components/MessagesPanel.tsx:266-275)
- **影响**：小白用户切到一个稍有历史的空间会看到「空房间」或残缺历史，怀疑数据丢失/产品坏了；这正是项目红线说的「界面撒谎」——把不完整当完整展示。
- **建议修法**：两种任选其一：① 给 MessagesPanel 加 key（在 Chat.tsx:992 <MessagesPanel key={`${workspace.id}:${activeThread?.id ?? 'main'}`} …/>）强制按房间 remount 重新 refresh；② 在 MessagesPanel 内把 refresh 的依赖与触发改为 [workspaceSlug, activeThreadId]（useEffect(()=>{refresh()},[refresh,workspaceSlug,activeThreadId]) 且 listMessages 带 to/from 或后续做房间级分页）。推荐 ①，最小且彻底。

#### P1-18 流式回复/pending 气泡出现时强制滚到底，打断用户向上翻阅历史
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：性能/真实用户路径/一致性　**置信度**：high
- **现象**：用户正在向上滚动阅读早前的对话，此时 AI 还在持续产出新消息、或「正在响应」气泡出现/消失，页面会被猛地拽回最底部，用户读到一半被弹走，反复发生时几乎无法回看历史。
- **复现**：在一个会产出长回复或多条连续回复的房间，发一条会触发流式/多气泡的消息，立刻向上滚动阅读历史，观察是否被反复拽到底。
- **根因**：MessagesPanel.tsx:517-521 的 useLayoutEffect 在 rows.length / pendingResponders.length / vanishedTurns.length 任一变化时无条件 virtualizer.scrollToIndex(rows.length-1,{align:'end'})，没有任何「用户是否已在底部」的 stick-to-bottom 判断（grep 全文件无 isAtBottom/atBottom/shouldAutoScroll）。聊天场景业界标准是只有当用户当前贴底时才自动跟随。
- **影响**：核心聊天体验受损：长回复或多 worker 并发时用户无法稳定回看历史，属于高频可感知的烦扰。
- **建议修法**：在 listRef 上监测滚动位置，维护 atBottom（scrollHeight-scrollTop-clientHeight < 阈值，如 80px）。仅当 atBottom 为真时才执行自动 scrollToIndex；用户离底时跳过，并可显示一个「↓ 新消息」回到底部按钮。

#### P1-19 虚拟化下自动已读：滚动经过未读消息即标记已读，违反「打开≠人真读了」红线
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：状态诚实性　**置信度**：medium
- **现象**：用户为了找某条消息快速滚动经过一批未读 agent→user 消息，或自动滚动把它们短暂带入视口，这些消息会被批量 POST 标记为「已读」，但人并没真正读。未读红点/计数随之清零。
- **复现**：房间里堆若干未读 agent 回复，快速滚动经过它们但不停留，观察未读计数是否被清零；或在不看屏幕时让新未读到达触发自动滚动，回来看是否已被标记已读。
- **根因**：web/src/lib/useScrollMarkRead.ts:70-95 (issue 标题误写为 web/src/hooks/...,真实路径在 lib/;行号内容吻合) 配合 web/src/components/MessagesPanel.tsx:517-521
- **影响**：未读状态对用户撒谎（显示已读其实没读），正是项目第二红线明确禁止的「把还没验证的乐观状态当已确认状态展示」。
- **建议修法**：提高门槛：threshold 设为 0.5~1（整条可见才算），并加最短停留时间（如进入视口后 setTimeout 800ms 仍可见才入队，离开则取消）；或仅对「滚动停止后仍可见」的行入队。auto-scroll 修好（见上条）后误触会显著减少，但门槛仍应加。

#### P1-20 markdown 链接未校验 scheme，依赖 rehype-sanitize 默认 schema 防 javascript:/data: href
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：安全/隐私　**置信度**：low
- **现象**：agent 输出（受 prompt-injection 影响）若产生 [x](javascript:…) 或 data: 链接，MarkdownLink 自身不拦截 href 协议，安全性完全押在 rehype-sanitize 默认 schema 上。
- **复现**：构造一条 agent 消息 body 含 `[点我](javascript:alert(1))`，确认渲染后 href 是否被 sanitize 去除（当前应已去除）；在脱离 sanitize 的场景复用 MarkdownLink 则会原样保留。
- **根因**：web/src/lib/markdownLinks.tsx:51-77 (MarkdownLink 无 scheme 白名单) 叠加真正的暴露点 web/src/routes/workspace/views/Context.tsx:474-479 与 web/src/routes/workspace/views/Ledger.tsx:407-412 (两处 ReactMarkdown 渲染 agent 写入的黑板内容却未挂 rehype-sanitize)；非仅 ChatMarkdown.tsx:38 的隐式依赖
- **影响**：当前低危（默认 sanitize 兜住了），但属于「安全靠隐式默认」的脆弱点，值得显式加固。
- **建议修法**：在 MarkdownLink/cleanMarkdownHref 里加显式 scheme 白名单（仅 http/https/mailto，其余降级为纯文本或丢弃 href），做到纵深防御不依赖单一 sanitize 默认值。

#### P1-21 方向『准备中(preparing)』窗口里，聊天空状态错误地显示『唤起 orchestrator』按钮，点击会产生双 orchestrator
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：状态诚实性 / 功能正确性(竞态)　**置信度**：medium
- **现象**：命名方向创建后到 git worktree 隔离完成（最长约 30s）这段时间，聊天区显示『闲置/无 AI』并给出『唤起 orchestrator』按钮。用户若在此窗口点击，会出现两个 orchestrator（一个在旧 shared cwd，一个被后端 reroot 到 worktree），即代码注释自己警告的 split-brain。
- **复现**：1. 创建命名方向。2. 在 worktree 隔离完成前（人为放慢 git 或大仓库）观察聊天区。3. 看到『唤起 orchestrator』按钮并点击 → 检查 listAgents 出现两个 orchestrator。
- **根因**：web/src/routes/workspace/views/Chat.tsx:278 (WorkspaceStatusStrip 的 hasMembers 判定缺少 activeThread.state==='preparing' 检查；revive 按钮渲染于 Chat.tsx:368-383，reviveOrchestrator 于 Chat.tsx:464-484；后端无守卫的 split-brain 竞态在 crates/swarmx-server/src/routes/workspaces.rs:1426 reroot_thread_orchestrator 与 rest.rs:2248-2261 的 cwd 解析)
- **影响**：诚实性：把『准备中』谎报为『闲置可手动启动』；功能性：用户点击触发重复 orchestrator，与后端 reroot 抢 cwd，文件写入 split-brain。
- **建议修法**：空状态渲染前先判断 activeThread.state==='preparing'：此时显示带 spinner 的『方向准备中…』占位，禁用/隐藏 revive 按钮（侧栏已经用 Loader2 表达 preparing，聊天区应保持一致）。activeThread 已在 Outlet context（ShellOutletContext 可下发），Chat 拿得到。

#### P1-22 is_sensitive 敏感名单是黑名单且覆盖严重不全，大量凭据/隐私文件可被读取
- **页面/域**：文件接口　**维度**：8. 安全/隐私（凭据泄露）　**置信度**：high
- **现象**：即便在 jail 内或开了 all=1，许多明显的凭据/隐私文件依然能被 /api/files/read 读出：~/.npmrc、~/.docker/config.json、~/.config/*（除 gcloud 外整目录都没挡，如 ~/.config/gh/hosts.yml GitHub token、~/.config/op、~/.config/Code 等）、~/.git-credentials 之外的 .git/config（内含带 token 的 remote url）、~/.bash_history / ~/.zsh_history、浏览器 Cookies SQLite、kubeconfig 若不在 ~/.kube、~/.terraform.d/credentials.tfrc.json、各类 *.token / *.secret 文件等。
- **复现**：curl 'http://127.0.0.1:7777/api/files/read?path='$HOME'/.config/gh/hosts.yml' 或 .../.npmrc → 直接返回明文 token；is_sensitive 对这两个路径返回 false（不含任何被枚举的目录/文件/名字模式）。
- **根因**：crates/swarmx-server/src/routes/files.rs:111-149
- **影响**：与发现1叠加，把『可读全盘』进一步坐实为『可读几乎所有凭据』。即使将来修了裸调用问题、把访问收进 workspace，开发者把项目放在 ~ 或把 ~/.config 加成 root 时，这些凭据仍会被任意 agent/依赖读走。
- **建议修法**：改为白名单/扩展名白名单思路而非黑名单：读接口默认只允许文本/代码类扩展名（.rs/.ts/.md/.json/.toml/...），或至少把 is_sensitive 扩成覆盖 ~/.config 整树、history 文件、*.token/*.secret/*.crt/*.cer/*.ovpn/.npmrc/.pypirc/.docker、浏览器 profile 目录等，并对『目录名以 . 开头的 dotfile 目录』默认更保守。长期看，文件浏览器不该承担读凭据的能力面。

#### P1-23 空目录 / 未选工作区 / 全是被过滤项时，列表区一片空白，没有任何空状态提示
- **页面/域**：文件页 Files　**维度**：空/加载/错误/边界态 + 小白路径　**置信度**：high
- **现象**：(1) 进入一个空目录：list.entries 为 []，files.tsx:197 的 .map 渲染 0 个按钮，loading 已结束、err 为 null、parent 可能也为根而隐藏 → 左栏完全空白，用户不知道是“空目录”还是“坏了”。(2) 没有任何工作区时（workspaces.length===0），header 不渲染 WorkspacePicker，wsId 为空 → useEffect 走 else 分支把 list 置 null（files.tsx:98-104），左右两栏都空，仅右栏有 previewHint，左栏纯白，没有“请先创建工作区”引导。
- **复现**：新装、还没建任何工作区时打开 /files；或选中工作区后进入一个空文件夹。左栏空白无任何文字。
- **根因**：files.tsx:197-218 只有 list?.entries.map，没有 entries.length===0 的兜底分支；files.tsx:93-105 wsId 为空时无任何占位文案。i18n 里也确实没有 files.empty / files.selectWorkspace 之类 key（grep 仅有 noWorkspaceName）。
- **影响**：小白第一次打开、或浏览到空目录时，看到大片空白会以为页面坏了/卡死，无法自助判断下一步。违反“下载→打开→立刻能用”的零摩擦要求。
- **建议修法**：list 非空但 entries.length===0 时渲染“此目录为空”空状态；wsId 为空且 workspaces.length===0 时在左栏渲染“尚无工作区，请先在主界面创建”引导（带跳转）。

#### P1-24 LiteLLM 兜底提示行对英文用户硬编码整句中文
- **页面/域**：用量与成本页 Usage　**维度**：一致性 / i18n　**置信度**：high
- **现象**：当后端返回 fallback.models > 0（总是成立，LiteLLM 表有上千模型）时，价目表区块下方那行说明在英文界面也显示整句中文「未列出的模型自动套用 LiteLLM 价格表兜底（覆盖 N 个模型）」。
- **复现**：语言设为 en，打开 /usage，看价目表标题下方那行说明是中文。
- **根因**：web/src/routes/usage.tsx:461-468（评审写的 web/src/pages/usage.tsx 路径有误，应为 web/src/routes/usage.tsx；内联中文 defaultValue 在 463-466 行）
- **影响**：英文用户每次打开 Usage 页都会看到一整句中文说明，且这是常驻可见（fallback.models 几乎永远 > 0）。比 saving 那条更显眼。
- **建议修法**：在 zh.json/en.json 的 usage 下补 pricingFallback（带 {{count}} 插值），删掉 usage.tsx 里的中文内联 defaultValue。

#### P1-25 切换目标状态失败时：界面已乐观改成新状态，但没有错误提示，且 reload 失败时假状态会悄悄留下（状态撒谎）
- **页面/域**：目标页 Goals　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户点某个状态按钮（如 active→complete），卡片立刻显示成「完成」并打绿勾。若后端 PATCH /api/goals/:id/status 返回 500 或网络断开，用户看不到任何错误，界面停留在「完成」。
- **复现**：1) 打开 /goals 选有目标的工作区；2) 停掉后端(crates/swarmx-server)或断网；3) 点任一状态按钮。现象：卡片立刻变新状态、无任何错误提示，且新状态一直留着。
- **根因**：web/src/routes/goals.tsx:124-142
- **影响**：用户以为已把目标标记为完成/阻塞/归档，实际后端根本没改。后续决策（停掉某方向、认为目标达成）建立在假状态上。这正是 CLAUDE.md 明令禁止的『显示成功但底层没成』。
- **建议修法**：给 setStatus 加 catch：失败时 setErr(友好文案) 并把 goals 回滚到调用前的快照（在乐观更新前先存 prev，catch 里 setGoals(prev)）；不要只靠 finally 的 load() 兜底，因为 load 自身也可能失败。

#### P1-26 工作区列表加载失败时，目标页伪装成「暂无目标」空态（吞掉了 useToolWorkspaces 暴露的 error）
- **页面/域**：目标页 Goals　**维度**：状态诚实性 / 空-加载-错误态　**置信度**：high
- **现象**：若 listWorkspaces 接口失败（后端没起/网络断），目标页不报错，而是渲染空态卡片『暂无目标。先创建一个…』，工作区下拉显示「—」。用户以为自己没有目标，实际是根本没连上后端。
- **复现**：1) 不启动 swarmx-server；2) 打开 /goals。现象：显示『暂无目标』而非『加载失败』。
- **根因**：web/src/routes/goals.tsx:54（解构遗漏 error）；触发态来自 web/src/lib/useToolWorkspaces.ts:34-37（catch 中 setError + setReady(true)）；空态误显落点 web/src/routes/goals.tsx:76-79 → 246-249。注意原报告把根因文件写成 web/src/hooks/useToolWorkspaces.ts，正确路径是 web/src/lib/useToolWorkspaces.ts。补充：tasks.tsx:42、terminal.tsx:48、usage.tsx:78 同样未消费 error，仅 files.tsx:56,167-174 做了正确处理——故此缺陷不止 goals 一页，但 goals 确实中招。
- **影响**：后端故障被伪装成『正常但无数据』，用户不会去排查连接问题，反而可能重复尝试新建目标（也会失败）。与项目红线『不许把失败默默吞掉显示成功/正常』直接冲突；其他工具页（按注释）本是要消费这个 error 的，目标页漏接。
- **建议修法**：在 goals.tsx 解构出 error 并在工作区下拉旁/列表区渲染明确的『工作区加载失败：xxx，请检查后端是否运行』错误态，与『暂无目标』空态区分开。

#### P1-27 错误提示直接把开发者向的原始字符串（含 HTTP 方法+路径+状态码）丢给用户看
- **页面/域**：目标页 Goals　**维度**：错误处理与失败恢复 / 真实用户路径（小白）　**置信度**：high
- **现象**：创建目标/加证据/改状态失败时，红色错误条显示的是类似 `POST /api/goals → 500: <原始错误>` 或 `PATCH /api/goals/xxx/status → 404: no such goal` 这种英文+路径的原文。
- **复现**：构造任一失败（如改一个已被删的目标状态），观察红条文案为带路径的英文原文。
- **根因**：web/src/routes/goals.tsx:89,118,304,327 (四处，非三处；其中 327 是"加证据"而非"改状态")
- **影响**：失败时用户拿不到可理解、可行动的提示，违反『出错要有清晰提示、能自助恢复』。同时把内部路由结构暴露在 UI 上。
- **建议修法**：捕获时判断 `e instanceof ApiError` 用 e.detail；并对常见状态码给 i18n 友好文案（如 404→该目标已不存在请刷新；网络错误→检查后端连接）。

#### P1-28 WebSocket 无 onerror、无重连、无手动重连入口——断线即死,只能切工作区或刷新
- **页面/域**：终端页 Terminal　**维度**：错误处理与失败恢复 / 真实用户路径 / 资源　**置信度**：high
- **现象**：连接建立失败或运行中断线(后端崩溃、sidecar 重启、网络抖动、被 IDLE_REAP 30 分钟回收)时,用户能看到的只有 xterm 里灰色一行 [session closed](terminal.tsx:99 的 onclose)。此后终端永久卡死:输入被静默丢弃,没有任何『重连』按钮。唯一恢复办法是把工作区下拉切到别的再切回来(触发 armed 重置 + effect 重跑),或刷新整页——对不懂内部机制的用户等于卡死。
- **复现**：1) 正常连上终端;2) kill 掉后端进程;3) 终端显示 [session closed];4) 在输入框打字无反应,界面无任何重连按钮,只能切工作区/刷新。
- **根因**：web/src/routes/terminal.tsx:75-99 (无 ws.onerror；onclose 仅写 xterm 不更新 React state)，叠加 effect 依赖 web/src/routes/terminal.tsx:115 无重连触发器
- **影响**：持久化 shell 是这个页面的核心卖点(注释强调跨导航存活),但一旦底层断开就彻底失能且无自助恢复,违反『出错能自助恢复而非只能重启』。对真实安装版用户:sidecar 偶发重启 = 终端永久黑屏卡死。
- **建议修法**：1) 加 ws.onerror 处理;2) onclose/onerror 时 setState 一个 disconnected 标志,在终端区覆盖一个『连接已断开 — 重新连接』按钮;点击时 bump 一个 reconnectNonce 触发 effect 重跑重建 WS(scrollback 会从服务端 replay 回来)。可选:对非用户主动关闭做有限次自动重连退避。

#### P1-29 WS 未 OPEN 时键盘输入被静默丢弃,用户以为没打上字
- **页面/域**：终端页 Terminal　**维度**：状态诚实性 / 边界态　**置信度**：high
- **现象**：在 WS 处于 CONNECTING 阶段(刚点连接、还没 onopen)或断线后,用户敲键盘,字符既不回显(终端是无回显的,依赖 PTY echo)也不报错,纯粹被丢进黑洞。用户会以为键盘坏了或卡了。
- **复现**：在断线([session closed])状态下持续敲键 → 无任何回显、无报错、命令全部丢失。
- **根因**：web/src/routes/terminal.tsx:102-104（协同根因：同文件 line 99 的 ws.onclose 仅写 [session closed]，未禁用 stdin / 未重连）
- **影响**：输入黑洞,无任何反馈,属于『一直转圈/无结果』式的不诚实交互的变体。对小白:刚进终端手快敲的命令可能丢一截,体验诡异且难排查。
- **建议修法**：非 OPEN 时不要静默丢弃:要么在连接前禁用/提示『正在连接…』,要么短暂缓冲待 onopen 后 flush;断线态下应由重连 UI 接管(见上一条),而不是让按键无声消失。

#### P1-30 「通用」三个开关全是死开关：写进 localStorage 但全代码库无人读取，纯装饰
- **页面/域**：设置页 Settings　**维度**：状态诚实性 / 功能正确性　**置信度**：high
- **现象**：用户在 设置→通用 里打开/关闭「启动时展开聊天窗口」「新消息桌面通知」「失败时停掉其它 agent」，开关动画正常、值也持久化了，看起来生效了。实际上三个开关对应用行为零影响。
- **复现**：1) grep -rn 'openMainOnLaunch|desktopNotify|killOthersOnFail' web/src crates web/src-tauri → 除 settings.tsx 自身与 i18n 文案外，零消费者。2) grep -rn 'new Notification|sendNotification|plugin-notification|isPermissionGranted' web/src → 无任何 OS 通知代码（通知系统只有应用内 NotificationPopover 铃铛）。
- **根因**：web/src/routes/settings.tsx:79-89 (字段声明+DEFAULTS) 与 settings.tsx:279-303 (GeneralPanel 渲染三个 ToggleRow)；缺失的消费侧：OS 通知无实现 (web/src-tauri 无 tauri-plugin-notification)、窗口启动无偏好读取 (web/src-tauri/src/lib.rs setup/CloseRequested)、失败级联无接线 (crates/swarmx-server/src/routes/rest.rs:1956 interrupt_all 仅手动端点)
- **影响**：这正是项目红线「界面撒谎」：用户以为配置了桌面通知/启动行为/失败联动，实际什么都没发生。尤其『新消息桌面通知』——用户关掉打扰指望静默，或打开指望后台弹通知，结果都落空，是会真实误导人的功能性谎言。
- **建议修法**：二选一：(a) 真正接线——desktopNotify 接 Tauri notification 插件在 agent 回复时发系统通知；openMainOnLaunch 在 src-tauri/lib.rs setup 里读该偏好决定 window 初始 show/hide 并改掉「关窗即退出」逻辑；killOthersOnFail 由后端 wake/registry 在 agent 异常退出时读取并级联 interrupt。(b) 在没接线前，把这三行从 GeneralPanel 删除或标注「即将推出」并 disabled，绝不能让一个看起来生效的开关其实是空操作。建议先做 (b) 止血。

#### P1-31 「启动时展开聊天窗口」开关与 Tauri 实际行为直接矛盾（关掉也照样开窗，且关窗即退出）
- **页面/域**：设置页 Settings　**维度**：状态诚实性 / 一致性　**置信度**：high
- **现象**：文案承诺『不开仅停在系统托盘 / off keeps it in the tray』。用户关掉该开关期望下次启动只待在托盘，结果每次启动仍然弹出 1440 宽的主窗口；而且一旦关窗，整个 app 直接退出，根本没有「留在托盘」这条路径。
- **复现**：读 web/src-tauri/src/lib.rs:42-109：setup 永远显示 window 且 CloseRequested→exit(0)，无任何 openMainOnLaunch / 偏好读取。
- **根因**：web/src-tauri/src/lib.rs:42-99 (setup 从不读取启动偏好、从不 .hide() 窗口) 与 web/src-tauri/src/lib.rs:106-110 (CloseRequested 直接 exit(0)，无 hide-to-tray)；前端死路在 web/src/routes/settings.tsx:282-283 写入无人消费的 openMainOnLaunch
- **影响**：承诺的托盘启动行为不存在，是明确的『显示成功但底层没成』。同时关窗即退出与『停在托盘』的心智模型自相矛盾，小白会困惑：我关掉了为什么还是弹窗、为什么一关就整个退出。
- **建议修法**：要么删掉这个开关与其文案；要么在 src-tauri 实装：启动时按偏好决定 window 初始 .hide()，并把 CloseRequested 改为 hide-to-tray（而非 exit），托盘菜单已有 Show/Hide/Quit 可配合。前端开关才有意义。

#### P1-32 隐私页「清空全部」是半截谎言：抹了 localStorage，但运行中的设置仍在内存里、下一次任意改动会把旧值写回
- **页面/域**：设置页 Settings　**维度**：数据完整性 / 状态诚实性　**置信度**：high
- **现象**：用户点「清空全部」→ 确认 → 弹 toast『已清除 N 项』。但页面上的主题卡高亮、语言选择、三个开关仍显示清空前的旧状态；并且只要随后再动任何一个开关，旧的主题/语言/开关值就被原样写回 localStorage，等于没清干净。
- **复现**：1) 切到 dark + English，开几个开关。2) 隐私→清空全部→确认。3) 观察：主题/语言/开关 UI 没变（仍 dark/En）。4) 再点任一开关→ 打开 devtools localStorage，swarmx:settings:v1 又出现且含旧 theme/lang。
- **根因**：PrivacyPanel.clearAllNow (settings.tsx:977-982) 直接 localStorage.removeItem 掉所有 swarmx:* 包括 swarmx:settings:v1，但 SettingsRoute 顶层的 settings state（126 行）和它的 useEffect([settings]) 持久化（132-140）完全不知情：clear 不修改 settings，所以既不会刷新 UI 显示，也不触发重写——直到用户下次 update() 任一字段，effect 用内存里的旧 settings 调 saveSettings 把整份旧配置复活。
- **影响**：『清除』这个不可逆危险操作的结果与界面展示不一致：toast 报成功，可主题/语言/开关并没回到默认，且会被静默复活。用户以为重置了实际没有，属于危险操作后的状态撒谎 + 数据不一致。
- **建议修法**：clearAllNow 清除后应同步把内存状态复位：setSettings(DEFAULTS)（并相应 setTheme(DEFAULTS.theme)/i18n.changeLanguage(DEFAULTS.lang)），让 UI 与 localStorage 一致；或把清除做成「清除后强制 reload」。至少不能让顶层 effect 用陈旧 settings 把刚清掉的 key 写回。

#### P1-33 ⚡ 手动唤醒按钮成功无反馈、失败被静默吞掉 —— 状态诚实性红线
- **页面/域**：调试页 Debug　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户点 agent 头部的 ⚡（手动唤醒，title 写「agent 卡住时点这个」），无论成功还是失败，界面上什么都不发生：没有 toast、没有 loading、按钮也不 disable。
- **复现**：进 /debug（需 VITE_ENABLE_DEBUG=1）→ 启动一个 agent → 点头部 ⚡ → 观察界面零变化；把后端 wake 接口改成返回 500，再点 ⚡ → 依旧零变化，只有 devtools console 有一行 warn。
- **根因**：web/src/routes/debug.tsx:156-163 wakeAgent()：成功分支什么都不做（仅 await，无 UI 反馈）；catch 里只有 console.warn("manual wake failed")，错误被静默吞掉。后端 web/.../routes/rest.rs:1121-1141 wake_agent 会返回 404（agent 已退出/不存在）或 500（deliver_manual_wake 失败），这些失败前端完全不展示。
- **影响**：这是「agent 卡住时的救命按钮」，恰恰是用户最需要确定性反馈的场景。点了没反应 → 用户不知道是已经发出唤醒、还是失败了、还是这个按钮本来就是死的，只能反复盲点或放弃。违反项目状态诚实性红线（不许把失败默默吞掉、不许点了不给结果）。
- **建议修法**：成功后用 AppToaster 弹「已发送唤醒」；catch 里把 (err as Error).message 弹成错误 toast（参考项目已有 AppToaster，而不是再加 alert）。并在请求期间给该按钮加 disabled 防重复点击。

#### P1-34 BreadcrumbsCard「近况」通栏完全没有 i18n，对所有语言硬编码中文
- **页面/域**：账本 Ledger 视图　**维度**：真实用户路径（i18n 漏翻）/ 一致性　**置信度**：high
- **现象**：下半部「近况」卡片：标题'近况'、副标题'worker 们最近的心跳(每完成一步会自动写)'、计数'N 个 worker'、空状态'还没有 worker 写过心跳…'——英文环境也全是中文，且这些字符串压根没走 useTranslation。
- **复现**：切 English → Ledger → 下方'近况'卡片标题、空状态均为中文。
- **根因**：web/src/routes/workspace/views/Ledger.tsx:315-367 (BreadcrumbsCard 未接入 useTranslation；硬编码中文在 328/331/335/341-342；并波及 fmtAgo 34/36/38、第 403 行，且 ledger.* key 在 zh.json/en.json 均缺失致上半部同样回退中文)
- **影响**：英文用户硬中文；术语'worker'/'心跳'对小白也偏技术。同一页面里上下半部的 i18n 处理方式还不一致。
- **建议修法**：BreadcrumbsCard 接入 useTranslation，把这 4 处文案抽成 ledger.breadcrumbs.* key 并补进两个语言包。

#### P1-35 后端 500 / 断网时 seed() 静默吞错,与「真正没有通知」渲染成完全一样的空态
- **页面/域**：通知中心 + 通知弹窗　**维度**：状态诚实性 / 错误处理与失败恢复 / 空加载错误边界态　**置信度**：high
- **现象**：后端没起、网络断、或 /api/message、/api/blackboard 返回 500 时,通知中心列表区显示和「确实一条通知都没有」一模一样的空态(铃铛图标 +「该分类暂无通知」)。用户被告知「没有通知」,但其实是没拉到数据。
- **复现**：停掉 swarmx-server(或断网),打开 /notifications。预期:应提示「加载失败/后端未连接,点击重试」。实际:显示空态「该分类暂无通知」,与真·空无法区分。
- **根因**：notifications.tsx 的 `seed()` (266-307) 整个 try 包住 `Promise.all([...])`,catch 块只有 `/* best-effort */` 完全吞掉。`requestEndpoint` 在任何非 2xx 都会 throw `ApiError`(http.ts:80-99 确认),所以 500 会进 catch → items 保持空 → 走 filtered.length===0 的空态分支(notifications.tsx:495-499)。NotificationPopover.tsx 的 `refresh()` (121-166) 同样静默 catch,后端挂时弹窗也只显示「Nothing here yet」。这违反项目状态诚实性红线:把「失败」显示成「成功(空)」。
- **影响**：用户在后端故障时被误导以为一切正常只是没消息,不会去重启/排查;也无从自助恢复(没有重试/错误提示)。通知是用户判断 agent 在干什么的主要窗口,这里撒谎影响很大。
- **建议修法**：seed()/refresh() 区分三态:加载中 / 加载失败 / 真空。catch 里 setState 一个 error 标志,空态分支先判断 error→渲染「加载失败 + 重试按钮」(重试调用 seed)。可复用项目里已有的 useSwarmFeedStatus()=='closed' 作为「后端连不上」的佐证一起提示。

#### P1-36 弹窗条目点击跳转用 workspace path 末 8 字符当 wsId,脆弱且可能跳错空间
- **页面/域**：通知中心 + 通知弹窗　**维度**：功能正确性 / 真实用户路径　**置信度**：medium
- **现象**：点弹窗里一条消息,跳转到 `/chat/{wsId}`,其中 wsId = `item.workspace.slice(-8)`(取 workspace 路径末 8 个字符)。若两个 workspace 路径末 8 字符相同(如 .../my-app/main 与 .../other/main,或末段恰好撞车),会跳到错误的空间;路径不含可识别 id 时则静默 fallback 到 /notifications。
- **复现**：—
- **根因**：web/src/components/NotificationPopover.tsx:213 (handleItemClick 内 `const wsId = item.workspace.slice(-8)`；item.workspace 来自 refresh :131 的 a.workspace=磁盘路径，应改用 a.workspace_id 反查 w.slug)
- **影响**：正常情况能用,但末 8 字符碰撞时点通知跳到别的空间(越权/串台风险的轻量版,至少是跳错),用户困惑。属功能正确性隐患。
- **建议修法**：改用真正的 workspace id 做关联:用 agentWorkspaces 反查时连同 workspace 的真实 id 一起存(workspaces 列表里有 w.id),或由后端在消息/agent 上直接给 workspace_id,不要从路径切片猜 id。

#### P1-37 新建向导「创建」按钮在提交全过程不禁用，双击会建出两个工作空间+两个 scout
- **页面/域**：首页/工作空间列表 + 新建空间向导　**维度**：功能正确性 / 资源泄漏　**置信度**：high
- **现象**：用户在新建向导填好名字+路径后双击（或网络慢时连点）「创建」，会创建两个一模一样的工作空间，各自启动一个 scout agent 烧 token，聊天侧栏冒出两个重复条目。
- **复现**：打开新建向导→填名字+合法路径→快速双击「创建」按钮（或在 devtools 把网络调到 Slow 3G 后连点）。观察 /api/workspaces 被 POST 两次，侧栏出现两个同名工作空间。
- **根因**：CreateWizard.tsx submit() (line 285) 全程没有 in-flight 防重入守卫。canSubmit (line 221-222) 只在 `scan` 被 set 后才变 false，而 `setScan` 直到 line 377 才执行——它前面要先 await 全部 validatePath(每个路径一个 api.filesList 网络往返, line 300-302)、再 await createWorkspace (line 337)、再 await 若干 addWorkspaceRoot POST (line 346-373)。这整个数秒级窗口里按钮 disabled={!canSubmit} 仍为 enabled。对比 Shell.tsx 的 onNewDirection 用了 creatingDirRef (line 229) 显式防双击，新建向导这条核心路径反而漏了。
- **影响**：核心入口产生重复工作空间+重复 PTY 进程，用户要手动删一个，且两个 scout 同时写 project.summary.* 黑板可能互相干扰；浪费 LLM token。
- **建议修法**：加一个 submittingRef (useRef(false))，在 submit() 开头 `if (submittingRef.current) return; submittingRef.current = true;`，在 try/catch 的 finally 里复位；或在点击后立即进入一个 'submitting' 视觉态让按钮 disabled。最小改法是在 submit 第一行设一个 isSubmitting state 并并入 canSubmit。

#### P1-38 删除工作空间失败被静默吞掉，只 console.warn，用户界面无任何提示
- **页面/域**：首页/工作空间列表 + 新建空间向导　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户在侧栏点删除→确认，若后端返回 500/网络断/已被并发删除(404)，弹窗已关闭，列表里那个工作空间依旧在，但没有任何错误提示。用户以为‘点了没反应’，再点一次或干脆放弃，完全不知道发生了什么。
- **复现**：—
- **根因**：web/src/routes/chat/Home.tsx:158-162 (catch 仅 console.warn+return);同源缺陷 web/src/routes/workspace/useWorkspaceShellData.ts:480-483;静默 UX 调用点 web/src/routes/workspace/WorkspaceSidebar.tsx:865-869 (fire-and-forget、未 await/catch、弹窗先于删除结果关闭)
- **影响**：违反项目状态诚实性红线：失败默默吞掉、界面不更新也不报错。用户无法自助判断该重试还是该重启。
- **建议修法**：在 catch 里调 toast.error(t('...删除失败...'), { description: (e as Error).message })；或整体用 toast.promise 包住删除动作。前面 killAgent 批量失败(line 146-150)同理至少应在全失败时提示。

#### P1-39 新建向导：折叠「高级」后，已填的非法附加路径仍卡死提交但错误信息被隐藏
- **页面/域**：首页/工作空间列表 + 新建空间向导　**维度**：空/加载/错误/边界态 / 真实用户路径　**置信度**：high
- **现象**：用户展开高级、加了一个附加目录、路径打错(目录不存在)→红字报错；然后把高级折叠起来准备先建主项目。此时「创建」按钮变灰点不动，但页面上看不到任何红字（附加行已被折叠隐藏），用户完全不知道为什么按钮是灰的，卡死。
- **复现**：新建向导→展开高级→添加目录→输入 /nonexistent/xyz(等 350ms 出红字‘目录不存在’)→折叠高级→观察‘创建’按钮变灰且页面无任何报错。
- **根因**：CreateWizard.tsx 中 invalidPath/checkingPath 基于 cleanDirs(来自全部 dirs，含被折叠的行) 计算 (line 218-219)，canSubmit 因此为 false；但渲染只展示 `advancedOpen ? dirs : dirs.slice(0,1)` (line 514)，折叠态下第 2 行起的错误 span 根本不渲染。错误状态对 canSubmit 生效、对用户不可见。
- **影响**：用户在核心入口被一个看不见的校验卡死，无文档可查、无提示，只能反复试或放弃——典型小白卡点。
- **建议修法**：三选一：(a) 折叠时把含 error 的附加行的错误冒泡到顶部 error 区或在「高级」折叠头上显示‘N 个附加目录有问题’的红色徽标；(b) 计算 canSubmit/invalidPath 时只统计当前可见(advancedOpen 或 index 0)的行；(c) 存在非法附加行时禁止折叠。推荐 (a)，既不丢校验又让用户看得见。

#### P1-40 design.md 的『通过/驳回』成功文案把『写了一个键』说成了『前后端 agent 会据此醒来开工』，存在状态撒谎风险
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：状态诚实性　**置信度**：medium
- **现象**：点『✓ 通过』后绿条提示『已写入 design.approved · 前后端 agent 会据此醒来开工』；点驳回后提示『已记录拒绝意见 · architect 会据此修订方案』。但若当前运行的不是 fullstack-feature-gated 那套 spell（没有任何 agent 订阅 design.approved/design.rejected），实际上没有任何 agent 会醒来，提示却斩钉截铁地承诺了下游行为。
- **复现**：—
- **根因**：BlackboardPanel.tsx:152-198 approveDesign/rejectDesign 只确认了『写入这个 blackboard 键成功』（refreshList 后才 setInfo，这一步是诚实的），但文案把后半句『agent 会醒来/会修订』当成既成事实陈述。wake.rs 的唤醒完全取决于是否有 agent 在订阅表里登记了 depends_on=design.approved；写键这一侧拿不到『有没有人订阅、有没有人真被唤醒』的回执（write_blackboard 只返回 id/sha/at）。按钮也无条件显示（只要选中 design.md），不校验当前 spell 是否是 gated 流程。
- **影响**：违反项目红线：把『还没验证的乐观下游效果』当成『已确认状态』展示。操作者在非 gated 流程里点了通过，看到『agent 会据此醒来开工』，会干等一个永远不来的结果，且无从判断是自己点错还是系统坏了。
- **建议修法**：文案降级为只陈述已确证的事实：『已写入 design.approved（若有 agent 订阅了该信号会被唤醒）』。更好的是让 write 响应或后续查询返回『本次写入唤醒了 N 个订阅者』（wake.rs 已知道 woke_anyone / 订阅快照），据此显示真实的 0/N；N=0 时提示『当前没有 agent 订阅该信号，不会触发任何动作』。


---

## 🟡 P2（130）

#### P2-1 GET /api/message?q=... 把用户原始字符串直接喂给 FTS5 MATCH，畸形查询直接 500 并把内部 SQL 错误回吐给前端
- **页面/域**：Blackboard 与消息接口　**维度**：错误处理与失败恢复 / 边界态　**置信度**：high
- **现象**：调用 GET /api/message?q=" 或 q=foo AND 或 q=* 这类含 FTS5 语法字符/不闭合引号/裸操作符的查询时，接口返回 500 INTERNAL_SERVER_ERROR，body 里是原始的 SQLite FTS 解析错误文本（如 fts5: syntax error near "..."）。正常的搜索框输入（带引号、括号、AND/OR/星号）会随机触发这个 500。
- **复现**：对本地 7777 执行 curl 'http://127.0.0.1:7777/api/message?q=%22'（q 为单个双引号），观察返回 500 与 fts5 语法错误文本。
- **根因**：crates/swarmx-storage/src/store.rs:1744 search_messages 里 `WHERE messages_fts MATCH ?1` 直接绑定用户传入的 query；FTS5 的 MATCH 右值本身是一套查询 DSL，畸形语法会让 SQLite 返回错误。crates/swarmx-server/src/routes/swarm.rs:33-38 list_messages 走 search_messages 分支，错误经 internal_err（swarm.rs:339）原样 to_string() 回前端，既给了 500 又泄露内部实现细节。注意这是 query 解析错误而非注入——参数化是对的，但 FTS DSL 校验缺失。
- **影响**：一旦前端把这个 q 接到搜索框（协议字段已就绪，只是当前 web/ 未接），用户每输入一个引号/括号就报错；即便现在，任何走 loopback 的调用方用合法字符也能稳定打出 500，且错误信息暴露后端用的是 SQLite FTS5。属于状态诚实性边缘：搜索失败应是「没结果/请换词」而不是「服务器内部错误」。
- **建议修法**：在 search_messages 前对 query 做 FTS5 安全化：要么把用户输入用双引号包成一个 phrase 并转义内部双引号（query.replace('\"', "\"\"") 再两端加引号，退化为纯短语匹配），要么显式 sanitize 掉 FTS 操作符；并把这一类错误在 handler 层映射成 400 + 友好提示（「搜索语法无效」）而非 500 原文回吐。

#### P2-2 POST /api/message/read 的 ids 是无上限 Vec<i64>，超量会撞 SQLite 变量上限直接 500（mark_read 未分块）
- **页面/域**：Blackboard 与消息接口　**维度**：边界态 / 性能 / 错误处理　**置信度**：medium
- **现象**：一次性提交超大 ids 数组（>32766 个，或在老 SQLite 上 >999 个）调用 /api/message/read，返回 500，错误为 SQLite 'too many SQL variables'。被标记已读的操作整批失败，且没有任何「部分成功/请分批」的提示。
- **复现**：构造一个含 40000 个整数的 JSON 数组 POST 到 /api/message/read（body<2MB），观察 500 与 'too many SQL variables'。
- **根因**：crates/swarmx-protocol/src/rest.rs:350-353 MarkReadRequest.ids 无长度约束；crates/swarmx-storage/src/store.rs:1785-1813 mark_read 把每个 id 拼成一个 `IN (?,?,...)` 占位符（store.rs:1794），加上 at_ms、to_agent 共 ids.len()+2 个绑定变量，一旦超过 SQLITE_MAX_VARIABLE_NUMBER 直接报错，没有按 batch 切分。这些 route 没有单独抬高 body limit，受 axum 默认 ~2MB body 约束，但 2MB JSON 足以塞进远超 32k 个 i64，所以该上限可达。
- **影响**：loopback 单用户场景概率低、影响有限，但属于「输入未校验 + 无分块」的真实健壮性缺口：调用方（含未来批量已读 UI 或脚本）一旦传入大批 ids，整批已读失败且只能看到一个不可自助恢复的 500。
- **建议修法**：在 store.mark_read 内对 ids 按固定 chunk（如 500）切分多条 UPDATE…IN 在同一事务里执行并汇总 RETURNING；或在 handler 层对 ids.len() 设上限并返回 400 友好提示。consume_wakes 用的是无变量列表的整表 UPDATE，不受影响，可作对照。

#### P2-3 MCP write/read_blackboard 把 agent 提供的 path 未编码直接拼进 URL，含 ?/#/空格 的 path 会被 URL 解析截断/错位
- **页面/域**：Blackboard 与消息接口　**维度**：功能正确性 / 一致性　**置信度**：medium
- **现象**：agent 通过 MCP 工具写/读一个 path 含特殊字符（如 'notes?v2'、'a b.md'、'x#frag'）时，crates/swarmx-mcp/src/tools.rs 直接 format! 拼 URL，? 之后被 server 当 query 丢弃、# 被截断、空格使 URL 非法，导致写到的 key 与 agent 以为的 key 不一致，或请求失败。
- **复现**：MCP swarm_write_blackboard path='a?b.md' content='x'，再 swarm_read_blackboard path='a?b.md'，观察 read 报 not found（实际写到了 'a'，'?b.md' 被当 query）。
- **根因**：crates/swarmx-mcp/src/tools.rs:527 与 crates/swarmx-mcp/src/tools.rs:560
- **影响**：blackboard 的 key 命名一旦带这些字符，写入路径与读取路径不一致，造成「写了读不到」或写到非预期 key；属功能正确性 bug，对依赖 blackboard 协作的 orchestrator/worker 链路有实际影响。
- **建议修法**：在 tools.rs 拼 URL 前对 path 的每个 segment 做 percent-encode（如用 url::Url 或 percent-encoding crate，保留 / 作分隔符其余编码），server 侧解码后 path_safe 校验不变。

#### P2-4 blackboard op-log 插入失败时返回 id=-1 但仍 Ok，HTTP 层把哨兵 id=-1 当正常 id 回吐，调用方无从得知这条写入未进历史
- **页面/域**：Blackboard 与消息接口　**维度**：状态诚实性 / 数据完整性　**置信度**：low
- **现象**：当 insert_blackboard_op 失败（SQLITE_BUSY 超预算、磁盘满等）时，write_blackboard 仍返回成功 JSON（routes/swarm.rs:331-336），id 为 -1。调用方/UI 看到的是「写入成功」，但这条写入不在 op-log 里：/api/blackboard 列表和 /api/blackboard-history 都查不到它，直到下次重启 reconcile_oplog_from_disk 才补回。
- **复现**：代码审查：write_blackboard 的 Err(e) 分支构造 id=-1 的 BlackboardOpRecord 并返回 Ok，routes/swarm.rs 将其 record.id 原样放入响应 JSON。
- **根因**：crates/swarmx-swarm/src/swarm.rs:606-631（哨兵 id=-1 来源）+ crates/swarmx-server/src/routes/swarm.rs:331-336（原样透传，无标记）。根因定位正确，但影响链需修正：真正的 agent 写入面是 crates/swarmx-mcp/src/tools.rs:553-581，它只读 sha256 不读 id；前端 web/src/components/BlackboardPanel.tsx:118-145 丢弃返回值——故"误导调用方"未成立。
- **影响**：内容不会丢（落盘是真的），但发现性/历史在重启前缺失，且接口返回的 id=-1 是个对调用方无意义甚至误导的值。这是一个有意识的权衡（注释写得很清楚），但 HTTP 层把 id=-1 当正常 id 回吐、不附带任何「op-log 未记录」标记，属于轻度状态不诚实。
- **建议修法**：保留落盘+广播策略，但在 HTTP 响应里对 id<0 的情况附加一个布尔标记（如 oplog_persisted:false）或把 id 置 null，让调用方知道这条写入暂未进入历史；或对 insert 失败做一次带退避的重试再降级。

#### P2-5 create_cron / update_cron 完全不校验 workspace_id 是否存在，可写入指向任意/不存在空间的任务，调度器随后每分钟无限重试复活并永久刷错误日志
- **页面/域**：Cron 接口　**维度**：功能正确性 / 数据完整性 / 资源泄漏　**置信度**：high
- **现象**：创建或编辑定时任务时，只校验 cron 表达式合法 + workspace_id/prompt 非空字符串（cron.rs:100-113 / 156-169），从不查 workspace 表。于是可以存下一个 workspace_id = 任意 UUID（甚至随手敲的字符串）的 enabled 任务。到点后调度器 run_job→revive_orchestrator→get_workspace_by_id 返回 None→Err，run_job 返回 Err，scheduler 只在 debug 级别打一行日志，且因为只在 Ok 分支 touch_cron_run（cron.rs:243、304-307），last_run_at 永不更新——于是**每个匹配的分钟都会重试一次、永远失败、永远不停**。
- **复现**：POST /api/cron body={workspace_id:"does-not-exist",name:"x",cron_expr:"* * * * *",prompt:"hi"} → 返回 200 ok:true 并入库；等到下一分钟，后端日志每分钟出现一行 `cron: skipped ... workspace not found`，last_run_at 始终为 null，任务永不停止重试。
- **根因**：crates/swarmx-server/src/routes/cron.rs:107 (create_cron 缺 get_workspace_by_id 校验) 与 crates/swarmx-server/src/routes/cron.rs:163 (update_cron 同) 为根因；放大因素在 crates/swarmx-server/src/cron.rs:243 (touch_cron_run 仅成功路径调用) 与 crates/swarmx-server/src/cron.rs:301 (去重依赖 last_run_at，失败路径不写故永不触发)。
- **影响**：数据完整性：库里能留下永远跑不通的「幽灵任务」。资源/性能：调度器对坏任务每分钟反复 list_agents + get_workspace 查询并尝试 spawn，长期空转；若表达式是 `* * * * *` 则每 30s tick 都打一次失败日志。用户体验：在前端只显示 orphaned 标记，但根因（一开始就写错 workspace_id）无任何即时反馈。
- **建议修法**：create_cron/update_cron 在校验后增加 `state.store.get_workspace_by_id(workspace_id)`（且过滤 deleted_at IS NULL），不存在则返回 400「workspace 不存在」。同时给 scheduler 的失败路径也写一次 last_run_at（或单独的 last_attempt_at）做退避，避免坏任务每分钟无限重试。

#### P2-6 get_workspace_by_id 不过滤 deleted_at，cron 复活路径可能对「已软删除的空间」重新拉起编排器
- **页面/域**：Cron 接口　**维度**：数据完整性 / 越权（跨空间资源生命周期）　**置信度**：medium
- **现象**：删除一个 workspace 时调用 disable_cron_jobs_for_workspace 把其下任务批量置 enabled=0（store.rs:1247+），但：(a) 手动「Run now」不检查 enabled，照样能对已禁用/已删空间的任务触发 run_job；(b) run_job→revive_orchestrator→get_workspace_by_id（store.rs:2384，SELECT 未带 `WHERE deleted_at IS NULL`）会把软删除行也查出来，于是用一个已经标记删除的 workspace 的 cwd 去 run_spell 拉起一个全新编排器。
- **复现**：1) 建一个 workspace + 一条指向它的 cron job；2) 删除该 workspace（软删，行仍在但 deleted_at 非空，且 disable 把 job 置 disabled）；3) 在 /cron 对该（orphaned）任务点 Run now；4) run_cron 不校验 enabled → run_job → get_workspace_by_id 仍返回该软删行 → revive_orchestrator 用其 cwd 拉起编排器。需确认 list_workspaces 是否已排除软删行造成 orphaned 标记。
- **根因**：crates/swarmx-storage/src/store.rs:2384-2410 (get_workspace_by_id 缺 `AND deleted_at IS NULL`); crates/swarmx-server/src/routes/cron.rs:231-249 (run_cron 触发前不校验 enabled / workspace 删除态); crates/swarmx-server/src/cron.rs:251-276 (revive_orchestrator 直接信任软删行的 ws.cwd); crates/swarmx-server/src/routes/rest.rs:2176-2191 (run_spell 仅对 None 报 not found，软删 workspace 照常放行)
- **影响**：已删除空间被意外复活：用户以为删掉空间就干净了，结果一个残留 cron job 被手动 Run（或若 disable 漏网仍 enabled）就把编排器重新拉起来，在那个 cwd 里跑 prompt。数据完整性/生命周期不一致。
- **建议修法**：get_workspace_by_id 增加 `AND deleted_at IS NULL`（或新增专用方法供 cron 用）；run_cron 在触发前若 job 所属 workspace 已删除/不存在，直接返回明确错误而非 revive；revive_orchestrator 对已删空间显式拒绝。

#### P2-7 tz_offset_minutes 无任何范围校验，可存任意 i32，污染调度时刻计算与前端时间渲染
- **页面/域**：Cron 接口　**维度**：输入校验 / 功能正确性 / 一致性　**置信度**：high
- **现象**：create_cron/update_cron 的 tz_offset_minutes、preview 的 offset 都是裸 i32，没有 [-720,840]（即 UTC-12..UTC+14）之类的合法时区范围校验。可以传 offset=999999999 之类，被原样入库并参与 `now_secs + job.tz_offset_minutes as i64 * 60`（cron.rs:297）与 next_after 的 `offset_min as i64 * 60`（cron.rs:173）计算，得到一个荒谬的「本地时刻」，调度会在用户完全意料不到的 UTC 时刻触发；前端 wallClock/formatRun 也会据此渲染出错误时间。
- **复现**：PUT /api/cron/:id body 里 tz_offset_minutes=500000 → 200 入库；列表 next_run 与实际调度时刻都按这个荒谬偏移计算，前端时间显示错乱。
- **根因**：crates/swarmx-server/src/routes/cron.rs:92-93 (CreateCronRequest.tz_offset_minutes)、139-147 (UpdateCronRequest.tz_offset_minutes)、60-67 (PreviewQuery.offset) 均无范围校验；唯一校验入口 crate::cron::is_valid (cron.rs:147-157) 只校验 5 个 cron 字段、不碰 offset。根因定位准确，无需修正。
- **影响**：功能正确性：任务在错误的 UTC 时刻触发，且 next_run 预览也算错，与用户「本地时间」的心智完全不符。前端展示与实际触发不一致（一致性/诚实性的边角）。虽然单机本地、攻击面有限，但正常前端永远只发浏览器真实 offset，出现异常值多半意味着客户端 bug 或脏数据，缺校验会让 bug 难以发现。
- **建议修法**：在 create/update/preview 校验 `-720 <= offset <= 840`（分钟），越界返回 400。

#### P2-8 preview_cron 把已知的 CPU 密集型 next_after 直接放在 async 执行器上跑，而同一函数在 list_cron 里专门用 spawn_blocking 隔离——前后矛盾
- **页面/域**：Cron 接口　**维度**：性能 / 一致性　**置信度**：high
- **现象**：list_cron 在 cron.rs:40 明确用 tokio::task::spawn_blocking 包住 next_after，注释（cron.rs:36-39）专门解释「这是 minute-by-minute 扫描、纯 CPU、不能堵 async 执行器」。但 preview_cron（cron.rs:74-82）调用同一个 next_after 却直接在 async handler 里同步跑，没有 spawn_blocking。preview 由创建表单每次输入防抖 300ms 后触发（cron.tsx:105-129），是高频调用点。
- **复现**：代码对照：cron.rs:40 用 spawn_blocking、cron.rs:77 不用，二者调用同一 next_after。
- **根因**：crates/swarmx-server/src/routes/cron.rs:77 直接 `crate::cron::next_after(...)`，未复用 list_cron 已采用的 spawn_blocking 隔离模式。两处对同一热点的处理不一致。
- **影响**：性能/一致性：next_after 因 date_matches 的「整天跳过」优化（cron.rs:181-184），最坏情况已被压到约 366 次天跳 + ≤1440 次分钟探测（如 `59 23 31 12 *`），单次约一两千次迭代，量级很小，所以不构成真正 DoS——这也是为何降为 P2 而非更高。但作者既然在 list 里认定它该进 blocking 池，preview 这个更高频的入口却漏了，属明确的不一致与潜在执行器阻塞隐患，应统一。
- **建议修法**：preview_cron 同样用 spawn_blocking 包住 is_valid+next_after，与 list_cron 保持一致。

#### P2-9 run_cron 手动触发不做 last_run_at 去重，可与调度器在同一分钟内重复投递同一 prompt（含并发多次点击）
- **页面/域**：Cron 接口　**维度**：并发 / 功能正确性　**置信度**：medium
- **现象**：调度器靠 `last_run_at / 60_000 == cur_min` 防止同一分钟重复触发（cron.rs:301-303）。但手动 run_cron（cron.rs:231-249）完全不查 last_run_at，run_job 成功后虽然会 touch_cron_run，但若用户在到点的同一分钟点了「Run now」，紧接着调度器 tick 时 last_run_at 已是本分钟→调度器会跳过——顺序无害；反之若先手动点两下（前端 runningId 只在单个组件内禁用按钮，多个标签页/快速双击仍可并发），run_job 之间没有任何互斥，会向编排器投递两条相同 cron note 并各自 deliver_manual_wake。
- **复现**：两个浏览器标签页同时打开 /cron，对同一任务几乎同时点「Run now」→ 服务端无互斥，编排器收到两条相同 cron note。
- **根因**：crates/swarmx-server/src/routes/cron.rs:231-249 run_cron 无去重/无锁；run_job(cron.rs:217-245) 也无幂等保护。前端 runningId 禁用（cron.tsx:510）只在单实例内有效，非服务端约束。
- **影响**：并发/功能正确性：编排器可能在极短时间内收到重复任务消息并被重复 wake，导致重复执行同一调度 prompt（对会改文件/发请求的任务尤其敏感）。属于较窄的竞态，正常单用户单击影响有限，故 P2。
- **建议修法**：若希望手动触发也幂等，可在 run_cron 里复用同一分钟 dedup，或对 (job_id) 加一个短期 in-flight 去重；至少在文档/UI 上明确「Run now 不去重、会立即额外触发一次」。

#### P2-10 valid_name 允许以连字符开头的名字，可作为 flag 注入到 claude/codex CLI 的参数位（install 与 uninstall 都中招）
- **页面/域**：MCP 接口　**维度**：安全 / 输入校验　**置信度**：medium
- **现象**：name 形如 `--scope`、`--help`、`-x`、`--global` 等全部通过校验（valid_name 只要求字符属于 [A-Za-z0-9_-]，不限制首字符）。这些值被放进 `claude mcp add <name> ...` / `claude mcp remove <name> ...` 的位置参数处，会被底层 clap 解析成 flag 而非 server 名，导致 CLI 行为被悄悄改变。
- **复现**：对 POST /api/mcp/uninstall 发 `{"name":"--scope","cli":"claude"}`：valid_name 通过 → 执行 `claude mcp remove --scope --scope user`，name 被解析成 flag，行为偏离预期。对比正常 `{"name":"context7"}`。
- **根因**：crates/swarmx-server/src/routes/mcp_admin.rs:153-159 (valid_name 首字符无约束)；uninstall 缺 allowlist 在 crates/swarmx-server/src/routes/mcp_admin.rs:445-456
- **影响**：在 loopback 单用户模型下危害有限（攻击者已是本机用户或经 CSRF/DNS-rebind 绕过 require_local_origin），但属于明确的输入校验缺口：构造特定 name 可让 remove 子命令解析出预期外的 flag，行为不可控。同时 uninstall 不校验 allowlist，意味着可对任意（合法字符的）user-scope server 名发起删除，超出「只管 chrome-devtools/context7」的设计意图（卸载误删面）。
- **建议修法**：valid_name 增加首字符约束：必须以 ASCII 字母或数字开头（拒绝 `-`/`_` 开头）。更稳妥：所有 CLI 调用在 name 与 `--` 之间显式用 `--` 终止 flag 解析（如能），并让 uninstall 也只接受 known() 白名单内的 name，避免对任意 server 发删除。

#### P2-11 uninstall 不限 allowlist，可删除用户任意 user-scope MCP（卸载误删/越权清理）
- **页面/域**：MCP 接口　**维度**：鉴权/越权 / 数据完整性　**置信度**：medium
- **现象**：mcp_uninstall 只过 valid_name，不调用 known()。任何能打到该端点的请求（本机网页经放行的 local Origin，或符合条件的 no-Origin 请求）都能 `claude mcp remove <任意合法名> --scope user`，删掉用户**手动配置的、与本应用无关的**其它 MCP server（如用户自己装的 filesystem、github 等）。
- **复现**：假设用户 ~/.claude.json 里有自配的 `filesystem` MCP。POST /api/mcp/uninstall `{"name":"filesystem","cli":"claude"}` → 通过 valid_name → 执行 `claude mcp remove filesystem --scope user`，删掉了与本应用无关的用户配置。
- **根因**：crates/swarmx-server/src/routes/mcp_admin.rs:445
- **影响**：前端 UI 目前只渲染 SERVERS 两项，正常用户点不到删除别的 server——但后端契约本身允许删任意 server 名，配合 CORS permissive + 本机任意网页可发 local-Origin 请求（require_local_origin 只校验 Origin 是不是 localhost，不校验是哪个 app），一个用户误访问的本地开发页面（任何 localhost:* 站点）就能调用此端点删光用户的 MCP 配置。属于越权删除 + 数据完整性风险。
- **建议修法**：mcp_uninstall 在 valid_name 之后加 `known(&req.name).is_some()` 校验，拒绝删除非本应用 allowlist 内的 server；让后端契约与「只管理我们装的那两个」的设计意图一致。

#### P2-12 mask_key 用 char 计数判长却用字节切片取尾 4，多字节 UTF-8 key 触发 panic（/api/mcp/status 返回 500）
- **页面/域**：MCP 接口　**维度**：功能正确性 / 边界态 / 错误处理　**置信度**：high
- **现象**：当从 ~/.claude.json 或 ~/.codex/config.toml 里 recover 出来的 key 末尾含多字节 UTF-8 字符（用户手改过配置、粘贴了含非 ASCII 的字符串、或 key 真含 Unicode）时，mask_key 在 `&k[k.len().saturating_sub(4)..]` 处可能切在码点中间，直接 panic。经 CatchPanicLayer 兜住后，GET /api/mcp/status 整个请求返回 500，MCP 页面加载失败（status 一直 null、转圈/报错）。
- **复现**：在 ~/.claude.json 的 mcpServers.context7.args 里写入 `["--api-key","abc世"]`（末尾是 3 字节中文）。GET /api/mcp/status → mask_key("abc世")：chars 计数=4 不进 short 分支……（注：需让 len>4 字节且尾 4 字节非边界，如 "abcd世"：字节 `&k[len-4..]` 落在「世」中间）→ panic → 500。
- **根因**：crates/swarmx-server/src/routes/mcp_admin.rs:304
- **影响**：虽然标准 API key 是 ASCII、正常路径不触发，但这是「读用户磁盘上任意内容再切片」的真实 panic 路径，且崩的是用户最常打开的 MCP 状态页（status 是页面首屏依赖）。一旦用户配置里有 Unicode，MCP 页面整片不可用，且报错信息（500）不会指向真正原因，难以自助恢复。
- **建议修法**：按字符而非字节取尾：`k.chars().rev().take(4).collect::<String>()` 反转回正序，或 `let tail: String = k.chars().skip(n.saturating_sub(4)).collect();`，杜绝字节边界 panic。

#### P2-13 API key 以明文命令行参数传给 claude/codex 子进程，进程表(ps)可见——本机其它进程/用户可窥探
- **页面/域**：MCP 接口　**维度**：安全/隐私　**置信度**：medium
- **现象**：安装需密钥的 server（context7）时，key 通过 `... -- npx -y @upstash/context7-mcp --api-key <KEY>` 作为子进程的命令行参数传递。在子进程存活期间，任何能读进程表的本地进程（`ps aux`、`/proc/<pid>/cmdline`、多用户机器上的其它用户）都能看到完整明文 key。
- **复现**：启用 context7 并填 key 的同时，另开终端 `ps aux | grep -i api-key` 或 watch 进程表，可见 `--api-key <明文>`。
- **根因**：crates/swarmx-server/src/routes/mcp_admin.rs:389-419
- **影响**：单用户本机模型下风险中等，但项目自我宣称「全程不显示完整 key、后端只回打码值」（McpPanel.tsx:12 + mask_key 的设计意图），而实际安装那一刻 key 又以明文进了进程表，与隐私承诺不完全一致。多用户/共享开发机或有恶意本地软件时，key 会泄露。
- **建议修法**：优先用环境变量或 stdin 把 key 传给底层工具（若 claude/codex 的 mcp add 支持 env 形式，如 `--env CONTEXT7_API_KEY=...` 或写入配置而非命令行）；至少在文档/UI 说明该 key 在安装瞬间对本机进程可见，不要对外宣称「全程不暴露」。需先用 ctx7 核实 claude/codex CLI 当前是否支持 env 方式注入 MCP key 再定方案。

#### P2-14 install/uninstall 失败被 upsert 的「忽略 remove 结果」掩盖，且无锁——并发点击会数据竞争
- **页面/域**：MCP 接口　**维度**：并发/数据完整性 / 资源　**置信度**：medium
- **现象**：对同一 server 的 claude/codex 两个开关快速连点，或多个本地页面同时操作，install 内部「remove 然后 add」两步与另一次请求的「remove/add」交错执行，读写的是同一个 ~/.claude.json / config.toml，可能互相覆盖、产出半截配置或丢失条目。前端 busy 锁只在单页面单次会话内生效，挡不住跨标签页/跨请求。
- **复现**：脚本并发 POST /api/mcp/install 与 /api/mcp/uninstall 同一 name 同一 cli 多次，观察 ~/.claude.json 是否出现条目丢失/重复/损坏。
- **根因**：mcp_admin.rs 全程无任何文件锁或互斥：install 的 remove(忽略结果)+add(mcp_admin.rs:433-434)、uninstall 的 remove，全部直接 spawn CLI 改同一份用户配置，存在 TOCTOU。前端 McpPanel 的 `busy` 状态（McpPanel.tsx:54,94-108）是组件本地状态，仅防同一页面并发，后端无防护。claude/codex CLI 自身改 JSON 是否原子也未知（很可能非原子读改写）。
- **影响**：并发写用户级 MCP 配置可能损坏文件或丢条目；配合上面的「先删后加」窗口，并发场景下更易把用户配置改坏。属于数据完整性隐患。
- **建议修法**：在 mcp_admin 层对「按 cli 的配置文件」加进程内互斥（如 per-cli 的 tokio Mutex），把 remove+add 串行化为一个临界区；或至少对同一 (cli,name) 的并发 mutate 排队。

#### P2-15 mcp.nodeTooOld 译文键在 zh/en 两个 locale 都缺失，英文用户永远看到写死的中文警告
- **页面/域**：MCP 管理页　**维度**：一致性　**置信度**：high
- **现象**：当 Node 存在但版本过低（v14 这类）时，McpPanel.tsx:159-169 渲染 `t("mcp.nodeTooOld", {…, defaultValue: "Node.js {{version}} 版本过低 …"})`。但 zh.json/en.json 的 mcp 块里都没有 nodeTooOld 这个键，于是无论中英文都回落到内联 defaultValue——一段写死的中文。英文界面的用户会看到突兀的中文句子。
- **复现**：切换到英文 + 用 Node v14 环境（或 mock env.node.adequate=false）→ 警告区出现中文「Node.js v14 版本过低 …」。
- **根因**：web/src/i18n/locales/zh.json:13 与 web/src/i18n/locales/en.json:13 的 mcp 块缺少 nodeTooOld 键（仅有 nodeMissing）；触发点 web/src/components/mcp/McpPanel.tsx:162-167 依赖写死中文的 inline defaultValue 兜底
- **影响**：i18n 漏翻 + 中英混杂，违反一致性维度；英文用户在一个本就让人焦虑（版本不够）的告警上看到看不懂的中文，体验更差。
- **建议修法**：在 en.json 和 zh.json 的 mcp 块补 nodeTooOld（带 {{version}}/{{min}} 插值）。英文给出英文文案，中文给出中文文案，移除对中文 defaultValue 的依赖。

#### P2-16 RuntimeChip 把「未安装」写死成中文，英文界面会露出中文
- **页面/域**：MCP 管理页　**维度**：一致性　**置信度**：high
- **现象**：Node/npm/uv chip 在「探测到不存在且无 version」时，McpPanel.tsx:459 直接渲染中文字面量 `未安装`（required 为真时）。英文用户在 Node 缺失场景的 chip 上会看到中文「未安装」。
- **复现**：英文界面 + 无 Node 环境 → Node chip 显示中文「未安装」。
- **根因**：web/src/components/mcp/McpPanel.tsx:459
- **影响**：i18n 漏翻、中英混杂；虽是小标签但出现在最关键的「Node 缺失」首屏告警区，破坏专业感与一致性。
- **建议修法**：用 `t("mcp.notInstalled", "未安装")` 并在两个 locale 补键。

#### P2-17 全局 busy 锁会禁用所有卡片的开关与「改密钥」，但只有被操作那张卡有 spinner，其余卡只是无声变灰
- **页面/域**：MCP 管理页　**维度**：一致性　**置信度**：medium
- **现象**：任一安装/卸载/改密钥进行中，busy 非 null，于是所有卡片的两个开关（McpPanel.tsx:283/290 `busy !== null`）和所有「改密钥」按钮（McpPanel.tsx:259 `disabled={busy !== null}`）都被禁用变灰。但只有正在操作的那张卡显示「同步中…」（McpPanel.tsx:293-298），其余卡片只是静默变灰、没有任何解释，用户会以为按钮坏了。
- **复现**：拨开一个开关，立刻去点另一张卡的开关或「改密钥」→ 灰着点不动且无任何说明。
- **根因**：McpPanel.tsx:54 单一全局 `busy` 状态 + McpPanel.tsx:259/283/290 用 `busy !== null` 一刀切禁用全部交互元素；缺少「为什么其它卡也点不动」的视觉/文案提示。
- **影响**：可访问性/一致性问题：被禁用控件无可见原因，用户困惑（尤其连点时）。功能上是安全的（防重复），但反馈不诚实/不一致。
- **建议修法**：要么把禁用收窄到当前 server（按 srv.id 维度），要么给其它被锁卡片一个轻提示（如统一一行「正在同步，其它操作暂不可用」）。

#### P2-18 optimize_prompt 的子进程 stdout/stderr 用 wait_with_output 全量读入内存，无大小上限
- **页面/域**：Prompt 优化 / Recording 接口　**维度**：资源泄漏 / 性能 / 边界态　**置信度**：medium
- **现象**：若 claude（或被错误配置成 claude 的其它二进制）在 45s 内持续向 stdout 喷大量输出，服务端会把全部内容缓冲进内存才返回。
- **复现**：理论复现：把 claude 插件 binary 指向一个长时间狂打 stdout 的脚本，触发优化，观察内存。
- **根因**：crates/swarmx-server/src/routes/rest.rs:2568-2569 `child.wait_with_output()` 把 stdout+stderr 无上限读入 Vec<u8>，仅靠 45s 超时（2569）兜底；haiku 小模型正常输出很小，但没有字节上限保护。
- **影响**：本地单用户、小模型、45s 限时下实际风险很低，是纵深防御缺口而非现实事故。
- **建议修法**：对 stdout 读取加一个合理上限（如 256KB，优化提示词本就该是短文本），超限即截断并按失败处理；或限制读取的 take(N)。

#### P2-19 get_recording 文档注释声称「streams the .cast file」，实际是 fs::read 整文件入内存后一次性返回
- **页面/域**：Prompt 优化 / Recording 接口　**维度**：一致性 / 性能　**置信度**：high
- **现象**：代码注释与行为不符；单个 .cast 最大可达 64MiB（DEFAULT_MAX_CAST_BYTES），每次播放回放都把整文件读进内存再发，而非流式。
- **复现**：读 recording.rs:55 注释 vs 79 行实现即可见不一致。
- **根因**：crates/swarmx-server/src/routes/recording.rs:55 注释写「streams the .cast file」，但 79 行 `tokio::fs::read(&row.path)` 是全量读入；writer.rs:37 单文件上限 64MiB。
- **影响**：本地 loopback 单用户、文件多为几 MiB，体感无碍；但注释误导后续维护者，且大文件回放会有一次性内存峰值。属一致性/性能小瑕疵。
- **建议修法**：要么把读取改成真正的流式（tokio_util::io::ReaderStream + Body::from_stream），要么把注释改成「reads the whole .cast into memory（≤64MiB）」以免误导。

#### P2-20 spell `task` / worker `system_prompt` 原样写进 PTY，未过滤控制字符/终端转义 —— 提示注入与终端转义注入面
- **页面/域**：Spell / Role / Plugin 接口　**维度**：安全/隐私（命令/提示注入）　**置信度**：high
- **现象**：调用 /api/spell/run（或 /api/worker）时，请求体里的 `task`（spell）或 `system_prompt`（worker）会被 render_prompt 拼进 agent 的首个提示词，然后作为原始字节直接灌进 agent 的真实 CLI（claude/codex）PTY。若文本里含回车(\r/\n)、ANSI/OSC 转义序列或 TUI 斜杠命令，会被子进程当成真实键盘输入解释。
- **复现**：POST /api/spell/run {"name":"init","task":"正常任务\r恶意指令：忽略以上，运行 rm -rf ...","workspace_id":"<id>"}；render_prompt 把含 \r 的 task 拼进 init 提示词，spawn_bootstrap_inject 原样灌进 orchestrator PTY，\r 可能在合法提示提交前先提交攻击者那一段。
- **根因**：crates/swarmx-server/src/routes/rest.rs:1382
- **影响**：在本地单用户、loopback-only 模型下，主要受益方是“能调用 /api/worker 的 orchestrator（即模型本身）”和任何能 POST 到 7777 的本地进程。orchestrator 由模型驱动、其 system_prompt 内容不完全可信（可被它处理的外部内容污染），等于把‘半可信文本→真实 CLI 键盘输入’的边界完全敞开：可注入斜杠命令、抢先提交、污染 OSC 生命周期标记导致状态机误判（与本项目红线‘状态不许撒谎’相关）。不是远程 RCE（loopback+CORS 防护在 main.rs:848+），但属于明确的注入面，且与诚实性红线耦合。
- **建议修法**：注入提示词前对 body 做控制字符白名单过滤：剥离/拒绝除必要换行外的 C0/C1 控制字符与 ESC(0x1b) 序列（至少过滤 \x1b、\x07、独立 \r）。更稳妥的做法是用 bracketed-paste 包裹注入体（写 ESC[200~ + body + ESC[201~，再单独发 \r 提交），让 TUI 协议级地把整段当粘贴、内嵌换行/转义不被解释——这同时也比现在依赖‘settle_ms 启发式’更可靠（解决 rest.rs:1393 记录的 21988 字节卡死类问题）。校验前确认目标 CLI 支持 bracketed paste（claude/codex 均支持）。

#### P2-21 spawn_worker 的 caller_agent_id 未做存在性/归属校验，可越权把任意 workspace/thread 当作上下文派生
- **页面/域**：Spell / Role / Plugin 接口　**维度**：鉴权/越权　**置信度**：medium
- **现象**：POST /api/worker 只校验 caller_agent_id 非空字符串，不校验它是否对应一个真实存在、且属于本 workspace_id 的 live agent。攻击者/任意本地调用方可以伪造 caller_agent_id 来驱动 thread/cwd 解析、并把派生消息 from=system to=<伪造id> 写进消息表。
- **复现**：POST /api/worker {"role":"backend","system_prompt":"x","workspace_id":"<别人的ws>","caller_agent_id":"任意不存在或他人的id"}；只要 workspace 存在、role 合法，就会拉起 worker 并把 wake/消息挂到伪造 caller 上，无任何归属拒绝。
- **根因**：crates/swarmx-server/src/routes/rest.rs:1500
- **影响**：loopback 单用户模型下风险被环境收窄，但仍是越权设计缺陷：一个本地非特权进程（或被污染的 orchestrator）可拿不属于自己的 agent_id/workspace 组合拉起 worker、把 wake 订阅挂到别的 agent 上、向任意 agent 投递 system 卡片，污染别的 space 的协作流。与审查维度‘越权访问别的 agent/space’直接相关。
- **建议修法**：在 spawn_worker 开头校验 caller_agent_id：用 store 查这个 agent 是否存在且其 workspace_id == req.workspace_id（live 优先查 registry，历史查 SQLite）。不匹配返回 400/403。同样在 send_message(to_agent=caller) 前确认 caller 真实存在，避免给幽灵 id 落库消息。

#### P2-22 run_spell 对内联 agent 的 depends_on 环检测有盲区：内联角色的 handoff_signal 被当作空，role↔role 环可能漏判
- **页面/域**：Spell / Role / Plugin 接口　**维度**：功能正确性　**置信度**：medium
- **现象**：一个 spell 若用纯内联 agent（只有 role + cli + system_prompt，无 role_ref，role 也不在 role 注册表里）声明 depends_on 互相指向，环检测可能放过它，导致运行期 worker 互等死锁（两个 agent 永远 INPUTS 阻塞）。
- **复现**：构造 spell：两个内联 agent A/B（无 role_ref，role 不在注册表），A.depends_on=["keyB"] 且其 prompt 写 keyB，B.depends_on=["keyA"] 且写 keyA。POST /api/spell/run，环检测因二者 handoff_signal 为空而通过，两 worker 运行期互等。
- **根因**：rest.rs:2099-2123 构建环检测输入时，role_handoff 只在 `state.roles.get(&resolved.role)` 命中时才填（rest.rs:2106）。内联 agent 的 role 不在注册表 → handoff_signal 留空 → detect_depends_on_cycles 把它当‘终端、不可被回指’，于是经由该内联节点的环无法被识别。注释 rest.rs:2104-2105 也承认 inline-only agent 留空 handoff。当前生产只跑单 agent 的 init spell，所以未爆发，但机制对未来多 agent 内联 spell 是错的。
- **影响**：当前零生产影响（只有 init 单 agent 在跑），属潜在正确性缺陷：若日后新增多 agent 内联 spell 且声明 depends_on 环，本应在 spawn 前 400 拒绝的死锁会被放过，运行期变成两个 agent 静默互等（PTY idle、无 token），靠 300s 超时后才 fail-loud——既慢又难诊断。
- **建议修法**：环检测的 handoff_signal 不应只来自 role 注册表。对内联 agent，应根据其 depends_on 引用的 key 反推/或在 spell 解析期就给每个 agent 一个稳定的 handoff_signal 表示（哪怕是 role 名约定），让 detect_depends_on_cycles 能看到内联节点产出的 key。或在 validate_manifest 阶段对‘内联 agent 间 depends_on’单独做一遍纯字符串环检测。

#### P2-23 set_task_status 对不存在的 agent_id 返回 {"ok":true} — 接口在撒谎成功（红线）
- **页面/域**：Tasks / Goals 接口　**维度**：状态诚实性/数据完整性　**置信度**：high
- **现象**：对一个已删除/不存在的 agent_id POST /api/tasks/:id/status，接口照样返回 200 {"ok":true}。前端 Kanban 卡片点 Block/Done/Archive 乐观地把卡片移到目标列，4 秒后下一次轮询把它默默弹回原状态，用户看到状态闪一下又跳回，没有任何报错解释。
- **复现**：1) GET /api/tasks 拿一个 agent_id；2) DELETE /api/agent/:id 或等其被清理；3) POST /api/tasks/<那个id>/status body {"status":"done"} 观察返回 200 ok:true 而 workers 表无变化。
- **根因**：crates/swarmx-storage/src/store.rs:922-935 set_task_status 里 conn.execute(UPDATE workers SET task_status=?2 WHERE agent_id=?1) 的受影响行数被直接丢弃，永远返回 Ok(())。handler crates/swarmx-server/src/routes/tasks.rs:103-104 据此回 {"ok":true}。对比同文件里 update_goal_status(store.rs:1051-1055) 正确地用 changed>0 区分并让 handler 回 404 — 同一项目里两套写法不一致。
- **影响**：用户操作显示成功但实际什么都没落库。命中项目红线：界面/接口不许显示成功但底层没成。卡片在轮询间隙被 reaper/kill 清理、或前端拿着 stale agent_id 重试时必现。
- **建议修法**：store.rs set_task_status 返回 Result<bool>(changed>0)；handler 在 false 时返回 404 {"error":"no such task"}，与 update_goal_status 对齐。前端 setStatus(tasks.tsx:76-80) try/finally 补 catch：失败时回滚乐观更新并提示。

#### P2-24 Kanban 接受 triage/ready 状态但看板没有对应列 → 任务凭空消失、计数对不上
- **页面/域**：Tasks / Goals 接口　**维度**：一致性/数据完整性/UX　**置信度**：high
- **现象**：后端 VALID_STATUSES 允许 7 种状态(含 triage、ready)，但前端看板 COLUMNS 只渲染 5 列(todo/running/blocked/done/archived)。一旦某任务状态被 override 成 triage 或 ready(API 完全允许)，该卡片不属于任何列、彻底从看板消失，但 header 的 {{n}} tasks 计数仍把它算进去 — 用户看到 12 tasks 却只数得出 9 张卡，且无法对该任务再操作(卡片不可见就点不到 Reopen)。
- **复现**：POST /api/tasks/<id>/status {"status":"ready"} → GET /api/tasks 该任务 status=ready → 打开 /tasks 页：计数+1 但无卡片显示，也无法对其 Reopen。
- **根因**：crates/swarmx-server/src/routes/tasks.rs:25-27 VALID_STATUSES 含 triage,ready；web/src/routes/tasks.tsx:32-38 COLUMNS 无这两列；byCol(tasks.tsx:85) 按精确 status 过滤且无 catch-all 兜底列。
- **影响**：状态词表(API 契约)与渲染词表不一致，导致数据丢失式的 UI(任务不可见且不可恢复)。属于自相矛盾+数据完整性问题。
- **建议修法**：二选一：(a) 后端 VALID_STATUSES 收敛到看板实际有的 5 个，拒绝 triage/ready；或 (b) 前端补 triage/ready 两列，或加一个"其它"兜底列收容任何未知 status，保证 sum(列)==tasks.length。

#### P2-25 create_goal / add_goal_evidence 对不存在的 workspace/thread/goal 返回 500 + 裸 SQLite 错误文案
- **页面/域**：Tasks / Goals 接口　**维度**：错误处理/输入校验　**置信度**：high
- **现象**：create_goal 只校验 workspace_id/objective 非空，不校验它们是否真的存在；传一个不存在的 workspace_id 或 thread_id，FK 约束(foreign_keys=ON)触发，handler 走 Err 分支返回 500 {"error":"FOREIGN KEY constraint failed"}。同理 add_goal_evidence 对不存在的 goal_id 返回 500 裸错误。本该是 400/404 的客户端错误被报成 500 服务端错误，且把底层 SQLite 文案透给前端。
- **复现**：POST /api/goals body {"workspace_id":"does-not-exist","objective":"x"} → 500 {"error":"FOREIGN KEY constraint failed"}。POST /api/goals/<bogus-id>/evidence 同样 500。
- **根因**：crates/swarmx-storage/src/connection.rs:36 (pragma foreign_keys=ON 的实际位置；报告误写为 crates/swarmx-server/src/db/connection.rs:36)。handler 缺校验/裸 500 的位置 crates/swarmx-server/src/routes/goals.rs:114-141、175-179、258-295 与报告一致。
- **影响**：错误码语义错误（客户端传错变成 500），错误信息泄漏存储实现细节，前端无法据 code 给出"工作区不存在"这类可恢复提示。
- **建议修法**：create_goal 先 get_workspace_by_id / get_thread 校验存在性，不存在回 404；FK 类错误归一化为 400/404 友好文案，不直接吐 e.to_string()。顺手订正 0003 迁移里"foreign_keys OFF"的过时注释。

#### P2-26 live-agent / spawn-depth 容量检查与实际 spawn 非原子(TOCTOU)，并发 /api/worker 可越过上限
- **页面/域**：Tasks / Goals 接口　**维度**：并发/资源泄漏　**置信度**：medium
- **现象**：spawn_with_bookkeeping 先读 registry.list().len() 与 cap 比较(rest.rs:556-566)，通过后才真正 spawn 并注册。多个 /api/worker(或 orchestrator 通过 MCP 并发委派)同时进入时，都读到 live<cap 再各自 spawn，实际 live 数会冲破 SWARMX_MAX_LIVE_AGENTS。spawn-depth 检查(rest.rs:1657-1689)同理是先查后插。
- **复现**：把 SWARMX_MAX_LIVE_AGENTS 设很小(如 2)，并发发起 >2 个 /api/worker，观察最终 live agent 数可短暂超过 2。
- **根因**：crates/swarmx-server/src/routes/rest.rs:556 (检查) 与 crates/swarmx-server/src/routes/rest.rs:883 (实际 registry.insert) 之间非原子；spawn-depth 同类问题在 rest.rs:1657-1689
- **影响**：fork-bomb 防护(F4)在高并发下不是硬上限而是软上限。每个超额 worker 是一个带 --dangerously-skip-permissions 的真实 CLI 进程，烧 PTY/RAM/API 预算。单用户本地场景概率与危害都有限，故 P2。
- **建议修法**：把容量判断与注册做成一次原子操作（在 registry 注册时由同一把锁内做 len 检查并拒绝），而非 check 后 act 两步；或用原子计数器 fetch_add 后超限回滚。

#### P2-27 Archive/Done 只改 task_status 标签、不停掉仍在运行的 worker 进程
- **页面/域**：Tasks / Goals 接口　**维度**：状态诚实性/一致性　**置信度**：medium
- **现象**：从看板对一个 killed_at=null(进程仍活)的 worker 点 Archive 或 Done，只写 workers.task_status 标签，进程继续跑、继续烧 token。卡片被归到 Archived 列，却同时挂着绿色 Running 活体圆点(tasks.tsx:186-191)，形成"已归档但还在跑"的并存观感。
- **复现**：spawn 一个 worker，进程运行中从 /tasks 点 Archive，观察卡片进 Archived 列但绿色 Running 圆点仍在、进程未退出。
- **根因**：crates/swarmx-storage/src/store.rs:922-935 (set_task_status，裸 UPDATE，crate 应为 swarmx-storage 而非 swarmx-server)
- **影响**：用户直觉上 Archive=收尾/关闭，但底层 agent 没停，预算继续消耗。好在确认弹窗文案(en.json tasks.confirm.archived.desc move out of the normal task flow)没有谎称会终止进程，属于诚实但易误解，故 P2。
- **建议修法**：要么在 Archive 时给出明确二选项"仅归档标签/归档并终止进程"；要么在 archived 列对 killed_at=null 的卡保留醒目的"仍在运行"标记并提供 Kill 入口，消除"绿点+归档"的认知冲突。

#### P2-28 前端 setTaskStatus 失败被 try/finally 无 catch 静默吞掉，乐观更新不回滚
- **页面/域**：Tasks / Goals 接口　**维度**：错误处理与失败恢复/状态诚实性　**置信度**：high
- **现象**：点 Block/Done/Archive 后，若 POST 返回 400(非法状态)或 5xx，前端 setStatus 用 try{await ...}finally{load()} 且无 catch，错误被吞；UI 先乐观把卡片移走，靠 finally 里的 load() 再拉回真值，期间无任何失败提示。用户看到卡片移动后又弹回，不知道是失败了还是后端状态变了。
- **复现**：构造一次会失败的 setStatus（如后端临时返回 500），观察前端无任何错误提示，仅卡片闪回。
- **根因**：web/src/routes/tasks.tsx:64-83 (具体为 76-80 的 try/finally 无 catch)
- **影响**：失败无反馈、用户无法自助判断与恢复，体验上是"操作像是没生效"。与上面 set_task_status 撒谎成功叠加时更隐蔽。
- **建议修法**：加 catch：失败时回滚乐观状态并 toast 报错（区分 400 非法状态 vs 5xx）。

#### P2-29 /ws/swarm 在 swarm 空闲时,客户端断开不会被察觉 → 连接/任务泄漏
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：资源泄漏　**置信度**：high
- **现象**：用户打开页面建立 /ws/swarm,然后关闭标签页/断网。如果此时 swarm 没有新事件产生(空闲),服务端那条 WS 连接对应的写循环 + broadcast::Receiver 会一直挂着,既不退出也不释放,直到下一次有 swarm 事件且 send() 失败才会清理。长时间空闲 + 反复开关页面会累积僵尸订阅者。
- **复现**：1) 启动后端;2) 用 websocat ws://127.0.0.1:7777/ws/terminal 之外的 /ws/swarm 连上;3) 保持 swarm 空闲(不发消息、不写 blackboard);4) kill 掉客户端进程或拔网线模拟无 FIN 断开;5) 服务端日志不会出现 'ws/swarm client disconnected',对应 task 持续存活。
- **根因**：crates/swarmx-server/src/routes/ws_swarm.rs:33-75 (根因定位完全正确，无需修正)
- **影响**：单用户本地工具影响有限,但属于真实的连接与任务泄漏:每个泄漏的订阅者还占着 swarm broadcast(容量 1024)的一个槽,极端情况下拖慢 Lagged 触发、长期占内存。
- **建议修法**：在主循环用 tokio::select! 同时等待 rx.recv() 与『reader_task 结束 / 一个表示客户端关闭的信号』,任一完成即退出并 abort 对方;或把结构改成与 pty_ws 一致(主循环读客户端、写在子任务里),并在写子任务里对 send 失败立即退出。也可加服务端定时 Ping 作为活性探测兜底。

#### P2-30 终端会话创建时持有 std::Mutex 跨同步 PTY fork/exec,阻塞所有终端会话与回收器
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：性能　**置信度**：medium
- **现象**：在 async 任务里持有进程级 std::sync::Mutex 的同时,执行同步的 openpty + fork/exec(spawn_shell)。新建一个浏览器终端期间,其它标签页的终端 attach/detach、以及每分钟的 idle 回收器全部被这把锁挡住;并且锁是在 tokio 工作线程上跨阻塞 syscall 持有的,会钉住该 worker 线程。
- **复现**：读代码即可确认:terminal_ws.rs:165 取锁,179 在持锁分支内调用同步 spawn_shell;229 才出作用域释放锁。
- **根因**：crates/swarmx-server/src/routes/terminal_ws.rs:165-229（持锁），阻塞调用在 179 行 spawn_shell → crates/swarmx-pty/src/lib.rs:67-104（openpty + spawn_command 同步 fork/exec）
- **影响**：单用户场景一般只是新建终端时其它终端操作短暂卡顿;但属于『持锁跨阻塞 syscall + 钉住 tokio worker』的反模式,在多终端/慢盘/被 ptrace 等情况下会放大为可感知卡顿。
- **建议修法**：把 spawn_shell 移到锁外:先在锁外完成 PtyBridge::spawn(失败直接返回),拿到 bridge/ring/bcast 后再短暂 lock 注册表做一次 insert;或用 tokio::task::spawn_blocking 执行 fork 并用 async Mutex / 更细粒度的锁。注意保持『replay 快照 + subscribe 在同一把锁下』的不丢不重不变量(可只对 ring 这一步保持原子)。

#### P2-31 终端 std::Mutex 用 .lock().unwrap(),一旦某任务 panic 投毒,所有终端会话永久不可用
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：错误处理与失败恢复　**置信度**：medium
- **现象**：进程级终端注册表与每会话 ring 都用 std::sync::Mutex 且全程 .unwrap()。只要任何一个持锁任务发生 panic(例如 pump 任务里的 ring 操作、reaper、或 handler 内持锁段),该 Mutex 被投毒,之后所有 .lock().unwrap() 都会连锁 panic,导致全部终端会话不可恢复,只能重启后端。
- **复现**：静态分析:terminal_ws.rs 中所有 registry()/ring 锁均为 std::sync::Mutex + .unwrap();任一持锁段 panic 即毒化,后续 lock 全 panic。
- **根因**：crates/swarmx-server/src/routes/terminal_ws.rs 多处:registry() 内 R 是 Mutex<HashMap>(65-71),取用处 82、165、212、287 全是 .lock().unwrap();ring 是 Arc<Mutex<...>>,取用处 169、197 也是 .unwrap()。std Mutex 的投毒语义意味着一次 panic 会让锁永久毒化。对照 pty 域用的是 parking_lot::Mutex(registry.rs:19),它不投毒——终端域用了会投毒的 std Mutex 却没处理 PoisonError。
- **影响**：正常路径不会触发,但任何一处持锁 panic 会把整个浏览器内终端功能打死且无法自愈(界面表现为终端再也连不上,且没有清晰提示),违背『用户能自助恢复而非只能重启』。
- **建议修法**：改用 parking_lot::Mutex(与 registry.rs 一致,无投毒),或对 PoisonError 做 .unwrap_or_else(|e| e.into_inner()) 恢复;同时审计持锁临界区里是否有可 panic 代码并尽量收窄。

#### P2-32 终端会话 id 是客户端任意提供且未校验,等价于一个授予完整 $SHELL 的能力令牌
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：安全/隐私　**置信度**：medium
- **现象**：/ws/terminal?session=<id> 的 session 完全由客户端给定,服务端不做任何校验/绑定。任何已通过 require_local_origin 的本地页面,只要复用/猜到一个已存在的 session id,就能 attach 到该 shell:重放其 scrollback(可能含命令历史/密钥)并注入键盘字节执行任意命令。
- **复现**：已读 terminal_ws.rs:141-174 与 web/src/routes/terminal.tsx:29-38 确认 session 由客户端提供、服务端零校验直接作为会话 key 复用。
- **根因**：crates/swarmx-server/src/routes/terminal_ws.rs:141-157 直接拿 q.session 作为 registry key,空则随机生成;reattach 分支(166-174)对任意命中 key 的会话照单全收(attached+1、订阅 bcast、replay 全量 ring),无 origin 绑定、无 workspace 绑定、无随机能力校验。正规客户端虽用 crypto.randomUUID()/工作区(web/src/routes/terminal.tsx:29-38)生成不可猜 id,但这只是客户端约定,服务端并不强制——session 实际承担了能力令牌的职责却没有令牌的不可伪造性。
- **影响**：在该项目既定信任模型(loopback + 同源)下危害有限,属于纵深防御缺口:一旦未来放宽到多源/嵌入第三方页面,或本地存在可被诱导发起同源 WS 的渠道,client 选定的 session 就成了劫持任意 shell(任意命令执行 + 读历史)的入口。
- **建议修法**：由服务端生成并返回不可猜的 session 能力(忽略/拒绝客户端自带的可枚举 id),或把 session 绑定到 workspace_id + 一个服务端持有的随机 secret;reattach 必须校验该 secret。至少在文档与代码里明确『session 必须服务端签发的高熵值』而非信任客户端。

#### P2-33 /ws/swarm 文档注释把 broadcast 容量写成 256,实际是 1024
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：一致性　**置信度**：high
- **现象**：ws_swarm.rs 顶部注释声称『lag past the broadcast channel's capacity (256) is disconnected』,但实际 swarm broadcast 容量是 1024。任何照注释推断 Lagged 阈值/背压行为的人都会算错。
- **复现**：对比 ws_swarm.rs:7-8 与 swarm.rs:214。
- **根因**：crates/swarmx-server/src/routes/ws_swarm.rs:7-8 写 256;实际通道在 crates/swarmx-swarm/src/swarm.rs:214 `broadcast::channel(1024)`。文档与实现漂移。
- **影响**：不影响运行,但误导后续维护者对滞后/丢事件阈值的判断。
- **建议修法**：把注释改为 1024,或改成引用常量避免再次漂移。

#### P2-34 /ws/swarm Lagged 分支构造并序列化了一个从不使用的 AgentState::Idle 事件
- **页面/域**：WebSocket：pty / swarm / terminal　**维度**：一致性　**置信度**：high
- **现象**：订阅滞后(Lagged)时,handler 构造了一个 SwarmEvent::AgentState{ __system__, Idle } 并序列化成 warn_payload,随后用 `let _ = warn_payload;` 直接丢弃,真正发给客户端的是另写的内联 error JSON。这段是无意义的死代码。
- **复现**：读 ws_swarm.rs:54-66。
- **根因**：crates/swarmx-server/src/routes/ws_swarm.rs:54-66:55-59 构造并 serde 序列化 warn_payload,60-64 实际发送的是手写的 error 字符串,66 `let _ = warn_payload;` 注释为『silence unused』把它丢掉。
- **影响**：无功能影响,纯属误导性死代码,每次 Lagged 还白做一次序列化分配。
- **建议修法**：删除 warn_payload 的构造与序列化,只保留实际发送的 error 帧。

#### P2-35 卡片操作按钮无 loading/disabled/防重入，4 秒轮询窗口内可重复点击与抖动
- **页面/域**：任务页 Tasks　**维度**：功能正确性 / 防重复点击 / 一致性　**置信度**：high
- **现象**：点完「完成」确认后，在后端返回前这段时间（以及 4 秒轮询刷新前），按钮没有任何 disabled / loading 态。用户可以连点多次、或对同一张卡连续点「阻塞」再「完成」，每次都弹确认框、每次都发一次 POST。卡片也没有「处理中」的视觉反馈。
- **复现**：1) /tasks 打开。2) 对同一张卡快速点「阻塞」确认、再点「完成」确认。3) 观察发出两次 POST，且无任何按钮禁用反馈；最终列归属取决于返回顺序。
- **根因**：web/src/routes/tasks.tsx:64-100 (setStatus 缺 in-flight 标志 + requestStatus 无条件弹框) 与 web/src/routes/tasks.tsx:234-244 (CardBtn 无 disabled/aria-busy)；竞态在持久层因 crates/swarmx-storage/src/store.rs:922-935 裸 UPDATE、last-write-wins 而真实暴露
- **影响**：重复 POST 增加无谓后端写；快速连点不同动作时，最后落库的状态取决于网络返回顺序，可能与用户最后点的不一致（竞态）。
- **建议修法**：给 setStatus 加 in-flight set（按 agent_id 记录正在提交的卡），提交期间把该卡按钮 disabled + 显示 spinner；requestStatus 在该卡 in-flight 时直接忽略。

#### P2-36 effective_status 可返回 triage/ready，但看板只有 5 个固定列，这类任务从所有列消失（计数与可见卡片对不上）
- **页面/域**：任务页 Tasks　**维度**：功能正确性 / 一致性 / 数据完整性　**置信度**：medium
- **现象**：如果某个 worker 的有效状态是 triage 或 ready，它不会出现在任何一列里（看板列 COLUMNS 只有 todo/running/blocked/done/archived），但页头的「N 个任务」计数仍然把它算进去。结果：页头说有 10 个任务，5 列加起来只有 8 张卡，两张「人间蒸发」，用户完全不知道它们去哪了。
- **复现**：1) curl -X POST /api/tasks/<agent_id>/status -d '{"status":"ready"}'。2) 刷新 /tasks。3) 页头计数 +1，但该卡片不出现在任何列，且无法复位。
- **根因**：web/src/routes/tasks.tsx:32-38 COLUMNS 写死 5 个 key；byCol（tasks.tsx:85）按精确 status 过滤，没有「其它/未知状态」兜底列。而后端 crates/swarmx-server/src/routes/tasks.rs:25-27 VALID_STATUSES 允许 triage 与 ready，set_task_status（tasks.rs:89-111）接受这两个值并落库，effective_status（tasks.rs:31-49）随后会把它们原样返回。页头计数 tasks.tsx:110-114 用的是 tasks.length（全量），与列内可见卡片数不一致。
- **影响**：通过文档化的 POST /api/tasks/:id/status 接口（或未来新增 triage/ready 按钮、或多端写入）把状态设为 triage/ready 后，这些任务在看板里彻底不可见且无法再操作（卡片没渲染就没有复位按钮），只能去数据库或换接口救。属于数据「看不见即丢失」的完整性问题。当前 UI 自身的按钮不会产生这两个状态，所以现网触发面有限，但接口契约与 UI 不自洽。
- **建议修法**：二选一：a) COLUMNS 补上 triage/ready 两列（i18n 键 tasks.status.triage/ready 已存在，翻译齐全），与后端 VALID_STATUSES 对齐；b) 加一个「其它」兜底列收纳所有不在已知列里的 status，保证任何有效状态都可见可操作；同时让页头计数与列内卡片总数口径一致。

#### P2-37 4 秒固定轮询永不停歇，标签页隐藏/失焦时照样打后端，且无可见刷新指示
- **页面/域**：任务页 Tasks　**维度**：性能 / 资源　**置信度**：high
- **现象**：/tasks 页面挂载后每 4 秒无条件 GET /api/tasks，即使窗口最小化、切到别的标签、或长时间无人看，也一直打。多 worker、多 workspace 场景下是持续的后端读放大。
- **复现**：1) /tasks 打开，devtools Network 过滤 /api/tasks。2) 切到其它标签放置 1 分钟。3) 观察后台仍每 4 秒发一次请求。
- **根因**：web/src/routes/tasks.tsx:58-62 useEffect 里 window.setInterval(load, 4000)，没有 document.hidden / visibilitychange 判断，也没有「窗口失焦暂停」。对照仓库近期 commit 1274d18『pause the cost poll while the tab is hidden』，usage 页已经做了隐藏暂停，这里没跟上，存在一致性缺口。
- **影响**：后台空转的网络与 DB 读；笔记本上多开标签时无谓耗电。功能不算坏，但与项目已确立的「tab 隐藏暂停轮询」惯例不一致。
- **建议修法**：参照 usage 页的实现，在 document.hidden 时跳过 load（或 clear/重建 interval），visibilitychange 恢复时立即 load 一次。

#### P2-38 卡片操作按钮（CardBtn）键盘可达但无 aria-label，点击热区过小，可访问性不达标
- **页面/域**：任务页 Tasks　**维度**：可访问性　**置信度**：medium
- **现象**：卡片上的「阻塞/完成/归档/复位」按钮字号 text-[10px]、padding px-1.5 py-0.5，点击热区远小于 WCAG 建议的 24×24/44×44；按钮只有文字、无 aria-label，对依赖语境的读屏用户缺少「对哪个任务做什么」的关联信息（屏幕上邻近的角色名并未通过 aria 关联）。看板列横向滚动容器也未提供键盘滚动提示。
- **复现**：用 VoiceOver/NVDA Tab 到卡片按钮，只读到「完成」无任务上下文；移动端实测点击热区偏小。
- **根因**：web/src/routes/tasks.tsx:234-244 CardBtn 仅渲染文字、无 aria-label/title；尺寸 px-1.5 py-0.5 text-[10px] 偏小。对比 TaskActivity.tsx:107-114 的 dismiss 按钮有 size-8 热区与 title，相对更好，可见标准在本页内不统一。
- **影响**：触屏/精细动作障碍用户难点中；读屏用户听到孤立的「完成」「归档」不知作用于哪个任务。
- **建议修法**：给 CardBtn 补 aria-label（含角色名，如 t('tasks.action.done')+role），增大热区至 ≥24px；或用 title 兜底。

#### P2-39 TaskActivity 的 ready 自动消失计时基于用户消息时间而非进入 ready 的时间，秒级完成时成功卡一闪即逝甚至不显示
- **页面/域**：任务页 Tasks　**维度**：功能正确性 / 一致性　**置信度**：medium
- **现象**：内联活动卡：若一个 task 创建时就已是 ready（fresh 全部 workable，Chat.tsx:947-956 直接以 ready 入栈，startedAt=用户消息时间），Chat.tsx:840-860 的 dismiss 定时器用 TASK_READY_DISMISS_MS - (now - startedAt)，当用户消息已过去 >4s 时 remaining<=0，卡片几乎立刻被 dismiss，用户根本看不到「✓ ready」那一下，等于没填上它要解决的 doomscrolling gap。此外注释口径不一致：TaskActivity.tsx:12 写「pending→spawning→ready」，Chat.tsx 一处注释说「pending 超过 15s」、另一处说「兜底 60s」，实际常量 TASK_PENDING_TIMEOUT_MS=60_000 是 60s，文档误导维护者。
- **复现**：发一条会立刻拉起且秒级 ready 的派活消息，观察「✓ ready」卡是否一闪而过或根本不显示。
- **根因**：web/src/routes/workspace/views/Chat.tsx:846 (remaining 基于 task.startedAt=用户消息时间 Chat.tsx:951)；ready 翻转处 Chat.tsx:912 与 Chat.tsx:941 均保留旧 startedAt 而无 readyAt，使根因不限于"直接以 ready 入栈"的 Chat.tsx:947-956；注释不一致在 Chat.tsx:766
- **影响**：派活很快完成（agent 秒级 ready）时，用户来不及看到成功反馈卡就消失，等于没填上它要解决的 doomscrolling gap；注释误导后续改动。
- **建议修法**：ready 的消失计时应基于「进入 ready 的时间戳」而非用户消息时间（给 task 记一个 readyAt 并据此算 remaining）；统一注释与常量口径（60s）。

#### P2-40 role 过滤器只过滤左侧成员列表，DAG 画布完全不受影响
- **页面/域**：依赖图 DAG 视图　**维度**：一致性 / 功能正确性　**置信度**：high
- **现象**：在左栏点某个角色(如 frontend)做过滤，左侧成员列表收窄到该角色，但中间的依赖图画布仍然显示全部角色的节点，前后矛盾。
- **复现**：—
- **根因**：roleFilter 只作用于 filteredAgents(Dag.tsx:398-404)，而 filteredAgents 仅用于渲染左侧 <ul>(Dag.tsx:624)。Canvas 收的是未过滤的 agents(Dag.tsx:713-714)，deriveHandoffEdges/deriveSpawnEdges/nodes 全基于 live(=未按 role 过滤)。所以「过滤」对图零作用。
- **影响**：用户期望过滤后图也聚焦该角色子图，结果图纹丝不动，会以为过滤器坏了或自己点错了；在 agent 多时尤其困惑。
- **建议修法**：要么把 roleFilter 一并传进 Canvas、对节点/边做同样过滤(注意边的两端被过滤掉时要丢弃)；要么明确把这个控件定位成「仅高亮/筛列表」并改文案。但当前「pill 看起来像全局过滤却只动列表」是误导。

#### P2-41 role 过滤指向已消失的角色时彻底卡死：pill 行被隐藏，无任何控件可重置
- **页面/域**：依赖图 DAG 视图　**维度**：空/边界态 / 错误恢复　**置信度**：high
- **现象**：URL 里 ?role=backend 残留，但此刻 backend 角色的 agent 全死了、且存活角色 ≤1 种时，左栏成员区只显示「暂无 agent(dag.empty)」，连一排过滤 pill 都不渲染，用户没有任何按钮能切回「全部」，只能手动改 URL。
- **复现**：workspace 里只剩 frontend、backend 两类各 1 agent → 过滤 backend → 等/杀掉 backend agent，使存活只剩 frontend 一类。此时 roles=[all,frontend] 长度 2，pill 行消失，列表空，URL 仍 ?role=backend，无控件可恢复。
- **根因**：web/src/routes/workspace/views/Dag.tsx:599 (pill 行 `roles.length > 2` 守卫) 与 Dag.tsx:291 (roleFilter 纯 URL 派生、无纠偏); 根因定位准确，无需修正
- **影响**：正常使用就能触发(过滤某角色 → 该角色 agent 跑完退出)，用户被困在一个空列表里，既看不到现存 agent 也无法清除过滤，只能重启/改地址栏——违反「打开即可用、零命令」。
- **建议修法**：两处之一：(1) 当 roleFilter 不在 roles 集合里时自动回落到 'all'(在 setRoleFilter 同源处或 useEffect 里纠偏)；(2) pill 行的渲染条件不要用 `roles.length > 2`，只要 roleFilter!=='all' 就始终渲染至少一个「全部」可点项作为逃生口。

#### P2-42 resume-all 整批失败时无任何用户反馈，只往 console.warn
- **页面/域**：依赖图 DAG 视图　**维度**：错误处理 / 状态诚实性　**置信度**：high
- **现象**：点「恢复所有暂停 agent」，若每个 agent 的 resume 都失败，界面无错误提示，弹窗关闭后看起来像成功了，但 agent 依旧暂停。
- **复现**：—
- **根因**：web/src/routes/workspace/views/Dag.tsx:443-457 (软失败 .catch 在 446-450 行 + 外层 try 缺 catch/setError)
- **影响**：操作者以为已恢复，实际全部仍暂停、不再自动唤醒，协作停摆；普通用户看不到 console，无从知晓。
- **建议修法**：统计失败个数，全失败或部分失败时 setError 提示「N 个 agent 恢复失败」；refresh 后 UI 仍显示暂停态也会暴露问题，但应主动给文字反馈而非只靠 console。

#### P2-43 edge 配色/图例硬编码十六进制，不随主题(暗色)走 CSS 变量，暗色模式下连线标签底色刺眼/看不清
- **页面/域**：依赖图 DAG 视图　**维度**：一致性 / 可访问性(对比度)　**置信度**：medium
- **现象**：依赖连线的标签背景写死 #FAFAF7(近白)，节点详情/画布其余部分用 surface-* 主题变量；在暗色主题下，边标签会顶着一块亮白底，spawn 线 #94a3b8、satisfied #2E8B57、waiting #C77A1F 也不随主题调整，对比度不可控。
- **复现**：—
- **根因**：web/src/routes/workspace/views/Dag.tsx:182,186,199 (内联 labelStyle/labelBgStyle/style 写死 hex，覆盖了 web/src/styles/global.css:362-369 本应生效的主题规则，因后者无 !important);图例硬编码 Dag.tsx:561,572,586
- **影响**：暗色主题下边标签亮白底块突兀、可能与文字对比不足；与项目主题化规范不一致。属体验/可访问性问题，非功能阻断。
- **建议修法**：把这些 hex 抽成 CSS 变量(或复用 roleHex 同源的语义色变量)，labelBgStyle 用主题的 surface-elevated 而非写死白；图例 stroke 也引用同一组变量，保证明暗一致。

#### P2-44 选中左侧列表里画布外的节点时，画布不自动平移/聚焦到该节点
- **页面/域**：依赖图 DAG 视图　**维度**：真实用户路径 / 一致性　**置信度**：medium
- **现象**：节点多、画布已缩放时，从左栏成员列表点一个当前视口外的 agent，节点高亮了但画布不动，用户看不到被选中的节点在哪，detail 面板弹出却找不到对应图元。
- **复现**：—
- **根因**：fitView 的副作用只依赖 [flow, nodes.length, edges.length](Dag.tsx:219-232)，选中只改 node.data.selected 不改数量，故不重算视口；onSelect 也没有 setCenter/fitView 到目标节点的逻辑。nodesDraggable=false 又使节点固定，用户只能手动平移找。
- **影响**：大图下选中后定位困难，体验割裂(列表选中 ↔ 画布无响应)；非崩溃，属可用性。
- **建议修法**：setSelectedId 时若来源是列表，调用 reactflow 的 setCenter/fitView 平移到该节点(node.position 已知)，让选中与视口聚焦同步。

#### P2-45 ModelPicker 选完模型/思考强度后浮层不关闭，易触发二次提交/二次重启
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：一致性 / 功能正确性 / 真实用户路径　**置信度**：high
- **现象**：在聊天顶栏点模型 pill，选「Sonnet」或某个思考强度后，下拉浮层不会关闭，停在原地。用户常见反应是「是不是没点中?」于是再点一次同一项或换一项——而每次有效切换都会弹确认框并重启在跑的队长、打断当前回复。
- **复现**：1) 进有 live 队长的方向;2) 点顶栏模型 pill;3) 选一个新思考强度——浮层不关;4) 再点另一个,第二个确认框叠上来/队长被二次打断。
- **根因**：web/src/components/ModelPicker.tsx:57（裸非受控 Popover）+ 132-167（MenuItem 仅 onClick、无 PopoverClose/disabled）；消费方 web/src/routes/workspace/views/Chat.tsx:622,689（modelBusy 静默早退）。注意确认框为单槽 modal（Chat.tsx:436,1280 + ConfirmActionDialog.tsx:30），不会叠加。
- **影响**：1) 范式不一致,用户预期「选了就关」;2) 浮层不关 + 无选中反馈,诱导重复点击;有 live 队长时每次有效切换都弹确认+killAgent+重跑 init spell(Chat.tsx:640-650),重复点会叠确认框、可能连续打断队长正在跑的回复;3) busy 期间点击被静默丢弃,违反「不许把还没生效的操作当无事发生」。
- **建议修法**：把 Popover 改成受控(useState open),在 MenuItem 的 onClick 里 onSet 之后 setOpen(false);或给每个选项包 PopoverClose。同时 busy 时给 MenuItem 加 disabled/视觉态,避免静默吞点击。

#### P2-46 SpellsLauncher 所有运行按钮必返回 400,组件完全跑不动(dev-only)
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：功能正确性 / 前后端契约　**置信度**：high
- **现象**：SpellsLauncher 里输入任务后点「✨ Auto」或「运行」,后端一律返回 400,界面红字「运行失败:POST /api/spell/run → 400: spell requires workspace context: pass workspace_id or caller_agent_id」。无论选哪个法术、填不填高级 workspace 路径都一样。
- **复现**：1) 用 VITE_ENABLE_DEBUG=1 跑前端;2) 打开 /debug;3) 顶部 SpellsLauncher 输入任意任务点运行;4) 红字 400 spell requires workspace context。
- **根因**：web/src/components/SpellsLauncher.tsx:86-91 (launch() body omits workspace_id/caller_agent_id) vs crates/swarmx-server/src/routes/rest.rs:2144-2173 (unconditional 400 when both absent)
- **影响**：组件作为元素级功能是死的——所有按钮都不可用。降级为 P2 仅因为它只挂在 routes/debug.tsx:268,而 debug 路由被 VITE_ENABLE_DEBUG==='1' 门控(web/src/lib/debug.ts:1),正式安装包里编译不进、用户碰不到。但它仍是真实损坏的代码,且 README 注释(types.ts:273)还宣称『top-bar SpellsLauncher』会传 workspace_id,与实现矛盾。
- **建议修法**：要么给 SpellsLauncher 接入当前 workspace 上下文、launch() 里带上 workspace_id(+ thread_id)再调 api.runSpell;要么既然已被聊天页的发消息自举/CreateWizard 取代,直接删掉这个废弃组件和 debug 里的引用,免得留个坏样板。

#### P2-47 AgentDrawer 暂停/恢复失败只 console.warn,UI 无任何错误提示
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：错误处理与失败恢复 / 状态诚实性　**置信度**：high
- **现象**：在 agent 抽屉点「暂停」或「恢复」并确认后,如果 interrupt/resume 请求失败,界面没有可见提示——按钮标签不翻转(因为 refreshInfo 后 paused 没变),用户看不出是『点了没反应』还是『后端拒绝了』。
- **复现**：1) 打开一个 agent 抽屉;2) 停掉后端;3) 点暂停并确认;4) 按钮没变、无任何报错,只有 devtools console 有一行 warn。
- **根因**：web/src/components/agent/AgentDrawer.tsx:242-254 togglePause 的 catch 只 `console.warn('toggle pause failed', e)`,没有 toast/inline 错误。比 wake 略好(按钮标签靠 refreshInfo 反映真实状态,不会假装成功),但失败时用户得到零解释。
- **影响**：失败时用户只能反复点或重启,没有自助恢复线索。普通用户看不到 console。属于『静默吞错』的弱化版。
- **建议修法**：catch 里补一个 toast.error(`暂停/恢复失败:${e.message}`);或用 toast.promise 包 togglePause。

#### P2-48 AgentDrawer 消息 tab 每来一条 swarm 消息就发 2 个 list 请求,无去抖
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：性能 / 资源　**置信度**：medium
- **现象**：agent 抽屉切到「消息」tab 后,只要该 agent 有任何收发消息事件,就立刻重新拉取——而且一次 refresh 发 2 个 HTTP 请求(from + to 各一次,客户端合并)。多 agent 高频对话时这会放大成密集请求。
- **复现**：1) 起一个多 agent 在跑的方向;2) 打开某 agent 抽屉切到消息 tab;3) 网络面板观察:每条相关 swarm 消息触发两条 /api/messages 请求。
- **根因**：web/src/components/agent/AgentDrawer.tsx:659-695 MessagesTab 的 useSwarmFeed onEvent 里,凡 message 事件且 from/to 命中本 agent 就无条件 refresh();refresh() 内 Promise.all 两个 listMessages(注释自己也承认服务端不支持 from OR to,只能两调,665-667 行)。没有去抖/节流,也没有增量追加(每次全量重拉 100+100 条)。
- **影响**：队长和 worker 来回密集对话时,抽屉开着的那个 agent 会被每条消息触发 2 次全量拉取,白白增加后端压力和前端重渲染。单 agent 量不大故非致命,但属于『轮询/请求放大』隐患,作者注释已自知。
- **建议修法**：给 refresh 加 200-300ms 去抖(leading+trailing),或改成把 swarm 事件里的 message 直接增量 append 进列表(事件已带完整 message 字段),只在首次 mount 拉一次历史。

#### P2-49 命令面板列出的可跳转项在后端 500/断网时静默变空,无错误态
- **页面/域**：全局外壳/命令面板/模型选择/Spell启动/Agent抽屉　**维度**：空/加载/错误/边界态 / 状态诚实性　**置信度**：medium
- **现象**：⌘K 打开时若 /api/agent 或 /api/workspaces 请求失败(后端没起/500/断网),面板里「工作空间」「唤醒 agent」两个分组直接不出现,看起来就像『当前没有工作空间/没有 agent』,而非『加载失败』。用户无法区分『真没有』和『拉取挂了』。
- **复现**：1) 停掉后端;2) 按 ⌘K;3) 工作空间、唤醒 agent 分组消失,无任何加载失败提示,像是真的没有。
- **根因**：web/src/components/CommandPalette.tsx:157-168 两个 api 调用都 `.catch(() => {})` 空吞,失败时 agents/workspaces 保持空数组;176/276/292 行又用 `workspaces.length > 0 / liveAgents.length > 0` 条件渲染,于是失败=空=分组消失。没有 error state,也没有 loading 态。
- **影响**：属于『把失败显示成空』的诚实性弱化。命令面板是导航/跳转的兜底入口,后端异常时它静默退化成『只剩导航/主题/设置』,用户摸不清为啥工作空间不见了。导航分组(写死的 BASE_NAV)仍在,所以不至于白屏。
- **建议修法**：给两个 fetch 加最简错误态:catch 里 setState 一个 error 标记,面板顶部渲染一行『工作空间/agent 列表加载失败,可重试』,而不是无声变空。

#### P2-50 apiBase.ts 在模块顶层无防护地读 window.location（行 24/27），node 环境（默认 vitest）import 即崩，使整条 API 层不可单测
- **页面/域**：前端 API 层 + 基址解析　**维度**：边界态 / 一致性 / 可测试性　**置信度**：high
- **现象**：任何（哪怕间接）import 到 apiBase.ts 的模块，在 vitest 默认的 node 环境下会在 import 阶段抛 `ReferenceError: window is not defined`，整文件/整测试套件直接挂，而不是某个用例失败。
- **复现**：在 web/ 下写一个 `src/api/http.test.ts` 里 `import { api } from './http'` 然后 `npx vitest run` → 套件在 import 阶段 ReferenceError: window is not defined。
- **根因**：web/src/lib/apiBase.ts:18-21 的 isTauriProd 已经用 `typeof window !== 'undefined'` 防护，但紧接着 line 24 `WS_HOST = isTauriProd ? '127.0.0.1:7777' : window.location.host` 和 line 27-28 `window.location.protocol === 'https:'` 在 isTauriProd 为 false 时会真去访问 window，没有同样的防护。web/vitest.config.ts 没有设 `environment:'jsdom'/'happy-dom'`，默认 node，无 window。
- **影响**：目前没有测试直接 import 它（grep 确认），所以是潜伏问题；但一旦给 http.ts / endpoints / 任何上层组件补单测，就会被这行卡死，且报错信息（ReferenceError）和真正想测的逻辑毫不相关，排查成本高。也与同文件已有的 typeof 防护自相矛盾。
- **建议修法**：把 line 24/27 的 window 访问也收进 `typeof window !== 'undefined'` 防护（或抽一个 `const loc = typeof window !== 'undefined' ? window.location : undefined` 统一兜底），node 环境给出安全默认（如 WS_HOST='127.0.0.1:7777'）。或在 vitest.config.ts 显式设 environment:'jsdom'。两者都做更稳。

#### P2-51 API 层完全没有统一错误处理/拦截层，错误是否被提示完全取决于每个调用点的自觉，已出现不一致与遗漏
- **页面/域**：前端 API 层 + 基址解析　**维度**：一致性 / 错误处理（附注明确要审）　**置信度**：medium
- **现象**：同一类失败，不同页面表现迥异：files.tsx 会把 403 翻成中文「越权被拦」、其余英文 raw；CreateWizard/WorkspaceSidebar 用 e.detail；而 wakeAgent 一族直接吞掉无提示；cron toggle/remove 既不 catch 也不 toast，只靠 load() 兜底重拉。没有任何集中拦截器把「网络断/5xx/403/404」映射成统一的用户文案与恢复路径。
- **复现**：对比 files.tsx 的 403 中文提示 与 CommandPalette wakeAgent 的静默吞错，同为后端拒绝、用户体验天差地别。
- **根因**：web/src/api/http.ts 的 api.* 只负责抛 ApiError，不含任何横切关注点（无 toast、无 retry、无网络态归一），requestEndpoint→request 这条链上没有 onError hook；每个 feature 自己 try/catch，风格各异（详见上面各 file:line）。
- **影响**：用户在不同页面遇到同一种后端故障，得到的提示语气/语言/可恢复性都不一样，部分页面甚至完全无提示；维护上每加一个新调用都要重复决定「错误怎么显示」，极易漏（wakeAgent 一族就是漏的证据）。
- **建议修法**：引入一层可选的统一错误处理：例如导出一个 `api.call(fn, { onError })` 或在 http.ts 里对特定 status（0=网络、403、5xx）产出标准化、可 i18n 的 detail，并约定调用点默认 toast。把「静默吞错」从默认行为改成需要显式 `silent:true` 才允许。

#### P2-52 apiBase.ts 注释把 macOS 说成唯一用 `tauri:` 协议的平台，与 Tauri v2 实际跨平台行为不符，易误导后续维护改坏安装版
- **页面/域**：前端 API 层 + 基址解析　**维度**：功能正确性（安装版三环境基址）/ 文档一致性　**置信度**：medium
- **现象**：注释（apiBase.ts:8-16）写「protocol === 'tauri:' 覆盖 macOS asset scheme，hostname === 'tauri.localhost' 覆盖 Windows/Linux」。但据 Tauri v2 官方文档：Linux(webkit2gtk) 生产环境同样走 `tauri://localhost`（protocol 也是 `tauri:`），只有 Windows 默认是 `http://tauri.localhost`。注释把 Linux 归到 hostname 那条是错的。
- **复现**：阅读 apiBase.ts:13-16 注释，与 Tauri v2 文档「custom protocol on macOS and Linux」对照即见出入（已用 context7 核实）。
- **根因**：web/src/lib/apiBase.ts:13-14
- **影响**：本身不致 bug（逻辑兜得住），但这是项目头号雷区（安装版基址）。错误注释会让后续维护者基于错误的平台模型去「优化」检测（例如以为 Linux 该用 tauri.localhost 而改成只判 hostname），从而在某平台打出连不上后端的安装包。属于埋雷型一致性问题。
- **建议修法**：按 Tauri v2 实际更正注释：macOS 与 Linux 生产均为 `tauri://localhost`（protocol `tauri:`），Windows 默认 `http://tauri.localhost`（hostname `tauri.localhost`，除非设 useHttpsScheme 仍是该 hostname）。强调检测靠 OR 两条同时兜住三平台，任何一条都不能单独删。

#### P2-53 uploadAttachment 走自己手写的 fetch、绕过 ApiError 与 AbortSignal，错误形态/取消能力与 api 层其余方法不一致
- **页面/域**：前端 API 层 + 基址解析　**维度**：一致性 / 资源管理 / 错误处理　**置信度**：high
- **现象**：粘贴/拖拽图片上传失败时，抛的是普通 `Error`（非 ApiError，无 status），调用方无法像别处那样按 403/404 分流；且该请求没有 signal 参数，组件卸载/用户取消时无法 abort，大图上传会一直挂到完成。
- **复现**：在编辑器粘贴一张大图、上传途中切走该视图 → 网络面板里上传请求不会被取消。
- **根因**：web/src/api/http.ts:372-385 uploadAttachment 因 body 是二进制没法走 request()，于是手写 fetch，但顺带丢了两样东西：(1) 失败只 `throw new Error(text)`（line 382），不是 ApiError；(2) 函数签名无 signal，fetch 也没传 signal。对照 optimizePrompt（line 362）就有 signal。
- **影响**：上传错误无法被统一识别与友好提示；大图/慢网下无法取消，组件已卸载请求仍在跑，属轻度资源/交互泄漏。一致性上是 api 层里的一个特例破口。
- **建议修法**：给 uploadAttachment 增加可选 `signal?: AbortSignal` 透传给 fetch；失败时构造并抛 ApiError(res.status, detail, msg)，与 request() 对齐。

#### P2-54 workspace_id 为 NULL 的 agent，其录像在 Replays 视图里彻底消失且无法访问
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：数据完整性 / 状态诚实性　**置信度**：medium
- **现象**：某些历史录像（尤其老版本创建的、或 spawn 时没写 workspace_id 的 agent 产生的）在每个 workspace 的 Replays 视图里都看不到，文件其实还在磁盘和数据库里。用户以为录像丢了。
- **复现**：构造一条 agents.workspace_id 为 NULL 但有对应录像的记录（或用早于 0004 迁移数据的库），打开任意 workspace 的 Replays —— 该录像不出现在任何 workspace。
- **根因**：web/src/routes/workspace/views/Replays.tsx:117 (set guard) + Replays.tsx:139-143 (strict equality filter)
- **影响**：录像数据对用户不可见也不可删（主用户面 Replays 是唯一入口；legacy /debug RecordingsPanel 不按 workspace 过滤还能看到，但那是开发者页，小白进不去）。属于界面对数据「撒谎」——明明有录像却显示为空/不存在。
- **建议修法**：见上

#### P2-55 中文界面下录像状态徽章与缩略图文案是英文（zh.json 漏翻 + 中英混杂）
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：一致性 / i18n / 真实用户路径　**置信度**：high
- **现象**：中文用户在 Replays 卡片上看到状态徽章显示英文「live」「completed」，缩略图角标显示「recording…」「ready to replay」，与同卡片上已翻译的「播放」「下载」并排，中英混杂、不专业。
- **复现**：切换语言到中文，打开 Replays，观察卡片右上徽章与缩略图角标为英文。
- **根因**：web/src/routes/workspace/views/Replays.tsx:252-254、343（破键唯一消费点；根因数据在 web/src/i18n/locales/zh.json:549-552 漏翻）。原根因误将 RecordingsPanel.tsx 列为消费方——RecordingsPanel.tsx:96 实为硬编码中文「● 实时／○ 已完结」，不消费 replays.* key。
- **影响**：中文是项目首要语言（用户全局规则强制中文）。核心功能页出现成片未翻英文，小白会困惑「live / completed」是什么意思。
- **建议修法**：zh.json replays.live→'● 实时'、completed→'○ 已完成'、recording→'▶ 录制中…'、ready→'✓ 可回放'（与 RecordingsPanel 里硬编码的『实时/已完结』措辞统一）。

#### P2-56 播放器底部明示「Esc 返回库」，但点进播放器后 Esc 被播放器吞掉、返回失效
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：状态诚实性 / 一致性 / 可访问性　**置信度**：medium
- **现象**：全屏播放器底部脚注高调写着 `Esc 返回库`。但用户一旦点击过播放区域（看回放时几乎必然），再按 Esc 不会返回 —— 提示与实际行为不符。
- **复现**：打开任意录像全屏播放器 → 用鼠标点一下终端画面（让焦点进入播放器）→ 按 Esc。观察未返回库；点画面外再按 Esc 才返回。
- **根因**：player.tsx:119-125 把 Esc→navigate(backTo) 监听挂在 window 上。但 asciinema-player 3.15.1 自身的 keydown 处理（bundle 内 onKeyDown）对 Escape 调用 setIsHelpVisible(false) 后执行 e.stopPropagation()+e.preventDefault()，且它通过 Solid 的事件委托绑定在 document 层。当焦点在播放器内，document 层委托先触发并 stopPropagation，事件到不了 window —— 路由的 Esc 返回逻辑收不到。仅当焦点在播放器外时 Esc 才生效。
- **影响**：界面承诺的快捷键在最常见情境下不工作，属于「显示能用其实不能用」的诚实性问题；键盘用户尤其受影响（鼠标用户还能点返回按钮，纯键盘用户被卡）。
- **建议修法**：把 Esc 监听改到捕获阶段：window.addEventListener('keydown', onKey, true)，在捕获阶段先于播放器的 document 委托拿到事件即可可靠返回（注意此时要判断不要和播放器 help 弹层冲突，可只在 help 未打开时返回，或捕获阶段直接返回）。或文案改为不承诺 Esc。

#### P2-57 播放实时(live)录像时无声播放被截断的快照，不提示「内容不完整」
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：状态诚实性　**置信度**：medium
- **现象**：对一条还在录制中(finalized_at==null)的录像点播放/进全屏播放器，asciinema-player 只加载点击那一刻已写入磁盘的字节，播到一半就停在快照末尾，用户以为录像就这么短/卡住了。
- **复现**：spawn 一个 orchestrator 让其持续输出，趁未退出时在 Replays 点该录像播放 —— 播到快照末尾即停，无任何不完整说明。
- **根因**：crates/swarmx-server/src/routes/recording.rs:79-98 (get_recording 非流式一次性整读，对 live 录像返回截断快照) 叠加 web/src/components/AsciicastPlayer.tsx:42-66 (create(src) 一次性加载、无 live 增长感知)；主用户面缺说明在 web/src/routes/workspace/views/Replays.tsx:245-255 与 web/src/routes/replays/player.tsx:198-202,232-237
- **影响**：用户可能误判录像内容缺失或播放器坏了。属于轻度状态不诚实（没主动说明这是不完整快照）。
- **建议修法**：live 录像的播放器附近显示一行说明（如『录制进行中，当前只能回放到此刻已写入的部分』），或对 live 录像禁用自动播放并提示『录制完成后回放更完整』。把 RecordingsPanel 里那句解释搬到 Replays/player。

#### P2-58 全屏播放器「原始 cast」分享链接在安装版里同样跨域 + 无 opener 能力，多半打不开
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：真实用户路径(安装版) / 错误处理　**置信度**：medium
- **现象**：播放器头部的分享/外链按钮(Share2，title『原始 cast』)在 Tauri 安装版里点了没反应，或弹出空白新窗口。
- **复现**：安装版打开播放器，点头部分享(Share2)按钮，观察无反应/空白窗。
- **根因**：web/src/routes/replays/player.tsx:211-219 (主用户路径；RecordingsPanel.tsx:118-125 仅 /debug 开发者路由可达，用户影响近零)
- **影响**：安装版用户点了无反馈（静默失败），不知道是没权限还是坏了。又一处『dev 能用、装包不能用』。
- **建议修法**：要么走 Tauri opener 插件 + 在 capabilities 里授权用 shell.open 打开外部 URL；要么把『原始 cast』也走 blob 同源在新标签预览/或直接复用下载逻辑。同时按钮失败要有可见反馈，别静默。

#### P2-59 Replays 列表错误态只显示后端原始 e.message，无重试按钮、断网时与空状态语义打架
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：空/加载/错误/边界态 / 错误恢复　**置信度**：high
- **现象**：后端 500 或断网时，Replays 顶部冒出一条红框显示英文/技术性原始报错（如 `GET /api/recording → 500: …` 或 `Failed to fetch`），用户看不懂，且没有「重试」按钮，只能手动点刷新图标或刷新页面。列表区此时仍走『empty 空状态』显示『没有匹配的录像』，与上方的报错并存，语义矛盾（到底是没有还是出错了？）。
- **复现**：停掉后端或断网，进入 Replays，观察红色原始报错 + 『没有匹配的录像』同时出现，且无重试按钮。
- **根因**：web/src/routes/workspace/views/Replays.tsx:123-125（原始 message 直抛）+ 219-228（错误红条与空态并列同屏、无三态区分/无重试 CTA）
- **影响**：小白看到技术报错且不知如何恢复；空状态与错误态语义打架，违反『后端没起/网络断时不该误导』。
- **建议修法**：区分三态：loading 显示骨架/加载中；error 显示友好文案+「重试」按钮(调 refresh)，并隐藏空状态；仅真正成功且 0 条才显示『暂无录像』。错误文案做 i18n，不直接抛 e.message。

#### P2-60 刷新按钮无 loading/防抖，swarm 退出事件无防抖触发双倍 list 请求
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：性能 / 资源 / 一致性　**置信度**：medium
- **现象**：快速连点刷新图标会并发打多次 listRecordings+listAgents；一批 agent 集中退出时，每个 agent_state=exited 事件都立刻触发一次 refresh（即两次 REST），短时间内重复请求成片。刷新时按钮无任何 loading/disabled 反馈。
- **复现**：连点刷新图标数次看 network 多次并发；或同时退出多个 agent 看 /api/recording 与 /api/agents 成串重复请求。
- **根因**：web/src/routes/workspace/views/Replays.tsx:107-126 (refresh 无 inFlight 去重+并行双请求), :184-192 (按钮无 disabled/spinner), :132-137 (exited 逐个无防抖触发 refresh); 服务端逐个广播证据 crates/swarmx-server/src/reaper.rs:57-85; dedupe 缺失证据 web/src/api/http.ts:163 与 :282 (二者均无 dedupe，与原文括注「listAgents 在别处有 dedupe」不符)
- **影响**：批量退出场景下接口被无谓打多次；刷新无反馈影响体感（不知道点没点到）。非致命但属轮询/重复请求类性能问题。
- **建议修法**：refresh 加 inFlight ref + 给按钮 disabled+spinner；useSwarmFeed 的 exited→refresh 做 200-500ms 防抖合并；可给 listRecordings 也接 dedupe。

#### P2-61 卡片「播放」悬浮提示纯 hover 显示，键盘/触屏不可见；搜索框与刷新缺 aria-label
- **页面/域**：回放视图 + 播放器 + 录制面板　**维度**：可访问性　**置信度**：medium
- **现象**：Replays 卡片缩略图上的『▶ 播放』遮罩只在鼠标悬停时出现(opacity-0 group-hover:opacity-100)，键盘 Tab 到该 Link 时不显示、触屏设备也看不到这个播放提示；搜索输入框只有 placeholder 没有 label，刷新按钮只有 title 没有 aria-label。
- **复现**：键盘 Tab 遍历 Replays 卡片，观察播放遮罩不出现；用屏幕阅读器读搜索框只念出 placeholder。
- **根因**：web/src/routes/workspace/views/Replays.tsx:176-182（搜索 input 缺可访问名，唯一真正成立的残余项）；报告对 263/184-192/258 的"无名/读屏拿不到信息"定性有误已驳回
- **影响**：屏幕阅读器/键盘用户拿不到等价信息；触屏用户看不到播放提示（但底部始终可见的『播放』按钮缓解了核心可达性，故非 P1）。
- **建议修法**：遮罩补 group-focus-within:opacity-100；input 加 aria-label={t('replays.search')}；刷新与缩略图 Link 加 aria-label。

#### P2-62 创建/保存/立即运行的错误信息直接把开发者向字符串 "POST /api/cron → 400: ..." 抛给小白用户
- **页面/域**：定时任务页 Cron　**维度**：错误处理与失败恢复 / 真实用户路径(小白) / 一致性　**置信度**：high
- **现象**：表达式非法或缺工作空间时，错误条显示形如 `POST /api/cron → 400: invalid cron expr ...`（含 HTTP 方法和路径），普通用户看不懂。
- **复现**：在创建表单填一个能绕过前端预览的非法表达式（或趁 300ms 防抖未跑完就点新建），后端 400，错误条显示 `POST /api/cron → 400: invalid cron expr \`...\``。
- **根因**：web/src/routes/cron.tsx:155 (另两处同因 cron.tsx:327、cron.tsx:355);开发向字符串拼装源头 web/src/api/http.ts:97
- **影响**：用户看到混着英文、HTTP 方法、内部路径的报错，既不友好也泄露后端实现细节；与项目其它页面错误展示风格不一致。
- **建议修法**：import { ApiError }，在三处把 `(e as Error).message` 改为 `e instanceof ApiError ? e.detail : (e as Error).message`，并对已知码（400/404/网络）映射 i18n 友好文案；invalidExpr 已有 i18n，可直接复用。

#### P2-63 相对时间("约3小时后"/"下次")基于渲染时一次性 Date.now()，长时间停留页面后越来越不准且永不刷新
- **页面/域**：定时任务页 Cron　**维度**：功能正确性 / 状态诚实性　**置信度**：high
- **现象**：页面打开后停在那里不动，‘下次 今天 09:00 · 约 3 小时后’这类相对描述不会随时间走动；几小时后甚至显示‘约 0 小时后’但任务尚未触发，或已过点仍显示未来时态。
- **复现**：打开 /cron，有 enabled 任务时记下‘约 N 小时后’，挂置 1 小时不操作，文案不变；手动改系统时间或等到过点，相对时态也不更新。
- **根因**：cron.tsx:131 与 cron.tsx:370（报告称的 CronJobForm.tsx:131 文件名有误——不存在独立的 CronJobForm.tsx，CronJobForm 是定义在 cron.tsx 内的函数；行号 131 正确）
- **影响**：‘下次运行’是这页的核心卖点，长时间挂着看会给出与真实时间脱节的相对时态，属于轻度状态不诚实（展示的‘还有多久’不再为真）。
- **建议修法**：用一个每 30–60s tick 的 state（如 setInterval 更新 nowTick）驱动重渲染，并在卸载时 clearInterval；或仅对相对时间用低频定时刷新。

#### P2-64 开关无 loading/disabled 防重，快速连点产生竞态请求
- **页面/域**：定时任务页 Cron　**维度**：功能正确性 / 性能 / 资源　**置信度**：medium
- **现象**：快速双击启用/停用圆点会连发两次 PATCH，且都基于同一份旧 j.enabled 计算目标值，最终状态依赖 load() 的最后一次返回，存在短暂错乱窗口。
- **复现**：对同一任务的开关快速双击，DevTools Network 看到两次 /api/cron/:id PATCH，开关短暂闪动后才稳定。
- **根因**：cron.tsx:451-462 开关按钮没有 disabled，也没有 in-flight 标志；toggle(j) 用闭包里旧的 j.enabled 取反（cron.tsx:339,341），同一渲染周期内的两次点击会算出相同目标并各自乐观翻转。
- **影响**：高频点击下界面状态短暂与后端不一致；虽有 load() 最终兜底，但中间态会闪烁/错位。
- **建议修法**：为正在切换的 job 维护一个 togglingId（类似 runningId），切换期间该开关 disabled，并从当前 jobs 的最新 enabled 取反而非闭包旧值。

#### P2-65 提交按钮的 canSubmit 在预览未返回(null)时即放行，可绕过非法表达式校验
- **页面/域**：定时任务页 Cron　**维度**：功能正确性 / 状态诚实性　**置信度**：medium
- **现象**：在 300ms 防抖预览尚未返回、或预览请求失败(被 catch 置 null)时，‘新建/保存’按钮可点，提交一个其实非法的表达式，全靠后端兜底再抛错。
- **复现**：在表达式框快速粘贴一个非法表达式并在 <300ms 内点新建，按钮不灰、提交后才报后端 400。
- **根因**：web/src/routes/cron.tsx:132（次因 web/src/routes/cron.tsx:121-123）
- **影响**：用户在‘看似没报错’的状态下提交非法表达式，再被后端 400（且错误文案是开发向字符串，见上条），体验割裂；前端的‘下次运行预览’这层护栏被静默跳过。
- **建议修法**：提交前若 exprPreview 为 null 先同步触发一次预览/校验（或用本地 describeCron 已能判定的有效性作为附加门槛）；区分‘预览失败’与‘尚未预览’，失败时不应等同于有效。

#### P2-66 工作空间下拉为空时只显示‘—’占位，无法创建，且不引导用户去先建工作空间
- **页面/域**：定时任务页 Cron　**维度**：真实用户路径(小白) / 空态 / 错误处理　**置信度**：medium
- **现象**：全新用户还没建任何工作空间就进 /cron，工作空间下拉只有一个‘—’，wsId 为空，canSubmit 永远 false，点不动新建，页面不解释为什么、也不给去处。
- **复现**：在没有任何工作空间的环境打开 /cron，工作空间下拉为‘—’，新建按钮恒灰，无任何引导。
- **根因**：cron.tsx:172 `workspaces.length === 0 && <option value="">—</option>`；wsId 因 useEffect(cron.tsx:101-103)无可填项而保持空，canSubmit 的 `!!wsId` 恒 false（cron.tsx:132）。整页没有‘请先创建工作空间’的空态引导或链接。
- **影响**：符合项目头号准则关注的‘第一次打开能不能不看文档就用’：定时任务依赖工作空间，但零工作空间时这页是个死胡同，新手会卡住且不知所措。
- **建议修法**：当 workspaces 为空时，在创建区显示明确空态文案+跳转去新建工作空间的入口，而不是只给一个不可用的‘—’下拉。

#### P2-67 列表加载失败(后端 500/断网)与‘空列表’无法区分，错误条出现但仍渲染‘还没有定时任务’空态
- **页面/域**：定时任务页 Cron　**维度**：空/加载/错误/边界态 / 状态诚实性　**置信度**：high
- **现象**：load() 失败时 setErr 显示错误条，但因 loading 已置 false 且 jobs 仍为初始 []，页面同时渲染‘还没有定时任务’的空态——给人‘连上了且确实没有任务’的错觉。
- **复现**：停掉后端后打开/cron，页面同时出现红色错误条和‘还没有定时任务。…’空态文案。
- **根因**：web/src/routes/cron.tsx:408-411 (空态渲染分支只看 jobs.length 不看 err；配合 load() 失败路径 cron.tsx:320-331)
- **影响**：后端没起/断网时，用户看到的是‘还没有定时任务’，可能误以为自己之前建的任务丢了（数据完整性恐慌），实则只是没拉到——属于轻度状态撒谎。
- **建议修法**：渲染时优先处理 err：err 非空时不渲染空态，而是显示‘加载失败，点重试’并提供重试按钮(调用 load())；区分‘已确认为空’与‘加载失败’。

#### P2-68 回复跳转/未读跳转目标若被房间或线程过滤掉则静默无反应
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：错误处理/边界态/一致性　**置信度**：high
- **现象**：点用户气泡里的「↩ #<id>」跳到父消息按钮、或顶栏「N 未读」跳转，如果目标消息不在当前可见集合内，按钮点了完全没反应、也没任何提示，用户以为按钮坏了。
- **复现**：在搜索框输入一个过滤词使父消息被筛掉，再点某条带「↩ #id」的气泡跳转按钮，观察无反应。
- **根因**：web/src/components/MessagesPanel.tsx:868 (jumpToParent 静默 return) 与 web/src/components/MessagesPanel.tsx:886 (jumpUnread effect 静默 return)；触发域错配根因在 MessagesPanel.tsx:418-422 (idToIndex←visible) + 359-410 (visible 过滤) + 268-270 (200 条窗口)，未读路径额外错配在 MessagesPanel.tsx:881 vs 885 及徽标源 web/src/routes/workspace/useWorkspaceShellData.ts:444-451
- **影响**：死按钮观感，用户无法理解为什么跳不过去；属于「失败被静默吞掉」。
- **建议修法**：idx==null 时给出反馈：toast 提示「该消息不在当前会话/已被筛选隐藏」，或临时清空 filter 再尝试定位；至少 console 之外要有用户可见提示。

#### P2-69 「优化」「重新发送」请求无 AbortController，组件卸载/快速切房间后 setState 于已卸载组件
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：资源泄漏/错误处理　**置信度**：medium
- **现象**：点「优化」(Sparkles) 或失败卡的「重新发送」后，趁请求在飞快速切走/关闭面板，回调里的 setBody/setError/setSending 在已卸载或已切房间的上下文上执行，开发期 React 报 setState-on-unmounted 警告，极端情况下把上一房间的草稿/错误写串。optimizePrompt 走 claude -p 可能要数秒，窗口不小。
- **复现**：发起「优化」后立刻切到另一个工作空间/方向，观察控制台 setState 警告，或优化结果是否写进了新房间的输入框。
- **根因**：web/src/components/MessagesPanel.tsx:723 (optimize 的 setBody(res.optimized)，调用点 720 未传 http.ts:362 支持的 signal；次要 setBody("") 在 626/663；resend 680-707 实为无害应剔除)
- **影响**：开发期噪音 + 偶发草稿/错误串房间，数据完整性轻微受损。
- **建议修法**：为这些异步动作引入 AbortController，在 draftKey（workspaceSlug+threadId）变化的 cleanup 里 abort；回调里以 mountedRef/最新 key 校验后再 setState。optimizePrompt 直接把 controller.signal 传进去。

#### P2-70 agentActivityById 把「晚到工具事件」回填进历史消息 thought_trace，可能在面板上无声重写已展示的思考摘要
- **页面/域**：对话主面板 MessagesPanel(核心)　**维度**：状态诚实性/性能　**置信度**：medium
- **现象**：一条已落地的 agent 回复，其下方「思考摘要」会在收到迟到的 swarm activity 后自动追加新步骤（最多保留尾部 12 条），用户看到的历史回复内容会在他不操作时悄悄变化。
- **复现**：让一个 agent 回复完成后，再延迟推送几条该 agent 的 ok/error activity（时间贴近完成时刻），观察该历史气泡的「思考摘要」是否自动多出条目。
- **根因**：MessagesPanel.tsx:281-330 的 effect 监听 agentActivityById，对 to_agent==='user' 且有 thought_trace 的历史消息，把 phase ok/error 且时间落在 trace 区间(+30s)的 activity 映射成 `完成工具: X` 步骤 push 进 summary 并 setItems。窗口判定 a.at<=completed_at+30000，跨回合/迟到事件可能误归到上一条消息；且每次 agentActivityById 引用变化都全量 map 一遍 items（200 条）。
- **影响**：历史消息内容被事后改写，用户对「这条回复当时说了什么」失去稳定锚点；归属窗口宽松时还可能把别的回合的工具贴错消息。轻度诚实性/正确性风险。
- **建议修法**：收紧归属：仅在 trace.completed_at==null（即该回复还在进行）时回填，已完成的历史回复不再追加；或把迟到步骤标注为「补充」而非混入原 summary。同时用 from_agent 维度的索引避免每次全量 map。

#### P2-71 新建命名方向时，git 隔离失败会让用户落进一个没有 orchestrator 的空房间，全程零提示
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：状态诚实性 / 错误处理与失败恢复　**置信度**：high
- **现象**：用户在侧栏点「+ 新方向」并填了名字 → 确认。前端导航进新方向，但聊天区是空的、没有任何 AI，也没有任何错误提示说明为什么。
- **复现**：1. 在一个 git 项目工作空间点「+ 新方向」填名字确认。2. 构造隔离失败（worktree 目标已被占用 / 制造 git 命令失败 / 断盘）。3. 观察：方向创建成功、导航进入、members 为 0、无 orchestrator、无错误 toast。
- **根因**：crates/swarmx-server/src/routes/workspaces.rs:1382-1391 (degrade 分支只 degrade_thread_to_shared、从不 spawn) 对"失败后无 orchestrator"这一机制定位正确；但经得起推敲的诚实性缺口实为"静默 degrade"——隔离失败只有 tracing::warn!(workspaces.rs:1383/1388) 加一个侧栏图标(见 workspaces.rs:1396-1402 注释)，主视图无任何"隔离失败/不再隔离"的可见提示。原报告"显示 ready 的空房间且无法对话"不成立：空房间已诚实显示"AI 未在线"并提供唤醒按钮+首条消息自愈。
- **影响**：命名方向在隔离失败时变成无 AI 的房间；界面把它显示成 ready（只是 degraded 图标），违反状态诚实性红线——显示『就绪』但实际没人能对话。
- **建议修法**：二选一：(a) 后端 degrade 分支也走一遍 spawn（在原 shared cwd run_spell init），与成功分支对称；(b) 前端不要无条件信任 preparing 就跳过 spawn——监听 thread_changed，若方向最终 degraded 且无 members 则补 spawn。缓解面：Chat 视图(Chat.tsx:368-383)的『唤起 orchestrator』按钮可让用户自救，但用户不知道为何要点，仍属诚实性问题；至少在 degrade 时给前端一个可见提示。

#### P2-72 新建方向 / 删除方向失败时静默吞错，对话框已关闭，用户得不到任何反馈
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：错误处理与失败恢复 / 状态诚实性　**置信度**：high
- **现象**：点「创建方向」确认后对话框立即关闭；若 createThread 后端 500/网络断，界面什么都不发生——没有新方向、没有错误提示，用户以为自己没点到。删除方向同理：点删除后若 deleteThread 失败，方向还在列表里但无任何说明。
- **复现**：断开后端或让 /api/workspaces/:id/threads 返回 500，点创建方向 → 对话框消失、无任何提示。
- **根因**：Shell.tsx:261-263 onNewDirection 的 catch 只有 console.warn('new direction failed')；Shell.tsx:279-282 onDeleteThread 的 catch 只有 console.warn 后 return。WorkspaceSidebar 在调用前就 setNewDirFor(null)/setPendingDeleteThread(null) 乐观关闭了对话框（WorkspaceSidebar.tsx:951-953, 1001-1004, 911-914），所以错误发生时已无承载错误态的 UI。
- **影响**：失败不可见、不可自助恢复（只能猜测重试）。注意 ManageRootsDialog 的 add/remove 是有 error 态展示的（WorkspaceSidebar.tsx:1113-1116, 1333-1335），说明项目里有正确范式，但 Shell 的方向创建/删除没沿用。
- **建议修法**：onNewDirection/onDeleteThread 失败时 surface 一个 toast 或在对话框内保留 error 文案后再关闭。最简方案：失败时不关对话框、显示红字错误（对齐 ManageRootsDialog 的 setError 模式）。

#### P2-73 侧栏与工具条移动端断点不一致(md vs lg)，768–1023px 同时出现常驻侧栏和冗余的『打开工作空间列表』按钮
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：一致性 / 真实用户路径　**置信度**：high
- **现象**：窗口宽度在 768px–1023px 之间时，左侧常驻工作空间侧栏已经显示，同时工具条顶部又出现一条移动端 header 带『打开工作空间列表』(PanelLeft) 按钮；点它会弹出一个全屏 Sheet，里面是和左边一模一样的侧栏。
- **复现**：把窗口拉到约 900px 宽，观察左侧栏与工具条顶部 PanelLeft 按钮同时存在。
- **根因**：web/src/routes/workspace/WorkspaceSidebar.tsx:503 (常驻侧栏 md:flex) 与 web/src/routes/workspace/WorkspaceToolbar.tsx:104 (移动 header lg:hidden) 断点不对齐；两者由 web/src/routes/workspace/Shell.tsx:437 与 :449 同层渲染，Sheet 冗余内容在 Shell.tsx:478-500
- **影响**：中等宽度窗口（常见的分屏/小笔记本）出现重复导航入口、视觉错位、点按钮弹出冗余全屏遮罩。
- **建议修法**：把两处断点统一：要么侧栏改 lg:flex（与移动 header 的 lg:hidden 对齐），要么移动 header 改 md:hidden。一致即可。

#### P2-74 工具条『跳转未读』按钮在 dag/ledger/replays 三个 tab 上是死按钮（点了无任何反应）
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：功能正确性 / 一致性　**置信度**：high
- **现象**：未读 badge +『跳转未读』按钮在全部 4 个 tab 的工具条里都显示。但只有在 chat tab 点击才会滚动到第一条未读；在 dag/ledger/replays tab 点击没有任何反应（不跳转、不切 tab、无提示）。
- **复现**：有未读时切到 /dag 或 /ledger，点工具条右侧未读 badge → 无任何变化。
- **根因**：WorkspaceToolbar 常驻于 Shell，未读跳转按钮无条件渲染（WorkspaceToolbar.tsx:225-236）。onJumpUnread 只是 setJumpUnreadTick(Shell.tsx:454)，该 tick 仅 Chat 视图消费（Chat.tsx:420,1001）。其它三个视图不挂载 MessagesPanel，tick 自增无副作用。
- **影响**：可点击元素无效果，违反『可交互元素必须真有行为』。用户在非 chat tab 点未读会困惑。
- **建议修法**：二选一：(a) 点未读时若不在 chat tab，先 navigate 到 chat tab 再 bump tick；(b) 仅在 chat tab 显示该按钮。推荐 (a)，符合用户『我要看那条未读』的意图。

#### P2-75 英文界面下 LIVE 离线态文案是写死的中文（i18n 漏翻）
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：一致性 / 真实用户路径(i18n)　**置信度**：high
- **现象**：切到英文后，当 swarm WS 断开时，工具条 LIVE 徽标变成『离线』，tooltip 显示『实时连接已断开,下面是最后已知状态』——在英文界面里是中文。
- **复现**：设置语言为 English，停掉后端让 WS 断开，hover LIVE 徽标 → tooltip 是中文。
- **根因**：web/src/routes/workspace/WorkspaceToolbar.tsx:198-200,212,218-220
- **影响**：英文用户看到中文，破坏 i18n 一致性。属低频（仅断网/服务重启时出现）但确定的漏翻。
- **建议修法**：在 en.json/zh.json 的 chat 下补 feedOffline、common 下补 offline 两个 key，移除内联中文 defaultValue。

#### P2-76 RootTree 内层展开/折叠按钮缺 aria-expanded（与工作空间行 chevron 不一致）
- **页面/域**：工作空间外壳 + 侧边栏 + 工具条　**维度**：可访问性　**置信度**：high
- **现象**：侧栏源根树里 peer 项目的展开/折叠按钮，屏幕阅读器读不出当前是展开还是折叠状态。
- **复现**：用读屏聚焦源根树的 peer 项目折叠按钮，听不到 expanded/collapsed 状态。
- **根因**：web/src/routes/workspace/WorkspaceSidebar.tsx:317-321
- **影响**：键盘/读屏用户感知不到树节点展开态。可访问性缺陷，轻微但确定。
- **建议修法**：给 RootTree 的 toggle button 补 aria-expanded={open}（行319 附近）。

#### P2-77 read_file 的二进制判定窗口(8KB)远小于读取上限(512KB)，非文本文件可能被当作乱码文本返回
- **页面/域**：文件接口　**维度**：1. 功能正确性 / 4. 边界态　**置信度**：medium
- **现象**：读一个前 8KB 内没有 NUL 字节、但后续是二进制的文件（如某些 UTF-16/无 NUL 头的二进制、或前缀是 ASCII 的容器格式），binary 会被判为 false，于是把整段（截断到 512KB）用 String::from_utf8_lossy 当文本 content 返回，前端会显示成一大片 � 替换字符的乱码；同时截断点 files.rs:276 是按字节切的，多字节 UTF-8 序列在 512KB 边界被切断也会产生末尾乱码。
- **复现**：构造一个前 9KB 全 ASCII、之后插入随机非 UTF-8 字节、总长 < 512KB 的文件，调用 /api/files/read → 返回 binary:false 且 content 含大量 �。
- **根因**：files.rs:278 `slice.iter().take(8192).any(|&b| b == 0)` 只看前 8KB 判二进制，而 files.rs:275-276 truncated/slice 用的是 MAX_READ_BYTES=512KB；二者窗口不一致。截断按字节而非按 UTF-8 字符边界。
- **影响**：纯体验/正确性问题，不涉及安全：偶发把二进制当文本返回造成乱码或前端渲染大段替换字符；不会崩溃（from_utf8_lossy 不 panic）。
- **建议修法**：用整个返回切片（而非仅前 8KB）做 NUL 检测，或引入更稳的二进制启发式；截断时回退到最近的 UTF-8 字符边界（如 str::from_utf8 失败位置或 char_indices 对齐）后再 lossy。

#### P2-78 /api/file(serve_file) 不受 workspace jail 约束，可被任意本地进程探测/读取任意位置的图片
- **页面/域**：文件接口　**维度**：8. 安全/隐私　**置信度**：medium
- **现象**：serve_file（rest.rs:2872）刻意不做 workspace 限制（注释 rest.rs:2870 明说『deliberately do NOT confine to workspace roots』），任何无 Origin 的本地进程可用 /api/file?path=任意绝对路径 读取该路径下的图片字节（限 25MB、需通过扩展名 + magic-byte 双重图片校验）。
- **复现**：curl -o /dev/null -w '%{http_code}' 'http://127.0.0.1:7777/api/file?path=/不存在' → 404；指向一张存在的 png → 200 并回图片字节，可据此枚举路径存在性。
- **根因**：设计选择：截图可能在任意位置，且认为图片不是机密，所以只做 host loopback + 图片真伪校验，无路径白名单（rest.rs:2872-2925）。
- **影响**：面比 files/read 窄（只能拿到真图片，且过 magic-byte），但仍允许任意本地进程：1) 通过 200/404/415 的差异探测任意路径是否存在（rest.rs:2882 缺失→404，非图片→415），形成文件存在性 oracle；2) 读取用户私密图片（含敏感信息的截图、相册）。
- **建议修法**：至少把存在性/类型差异收敛成统一错误以减少 oracle；对私密图片场景考虑同样纳入 workspace jail 或要求来源是用户交互（带本地 Origin）的请求，而非无 Origin 的子进程。

#### P2-79 openFile 无防陈旧响应守卫：快速切文件会把旧文件内容贴到新文件标题下（状态撒谎）
- **页面/域**：文件页 Files　**维度**：状态诚实性/功能正确性/竞态　**置信度**：high
- **现象**：连续快速点击文件 A 再点文件 B：若 A 的 filesRead 比 B 晚返回（A 是 512KB 大文件、B 是小文件），右栏标题显示 B 的文件名/大小，但正文内容是 A 的。用户看到的预览与左侧选中项、与标题栏不一致——界面在“撒谎”。
- **复现**：选中工作区→左栏快速点击一个大文本文件（接近 512KB）再立刻点一个小文件→右栏标题是小文件名，正文却是大文件内容（或反之，取决于到达顺序）。
- **根因**：web/src/routes/files.tsx:118 (openFile 的 setPreview 无 path/requestId 守卫，await 后无条件写入；open 的 web/src/routes/files.tsx:80 同理)。修正症状：标题与正文均取自同一 preview 对象(files.tsx:253-256,266-268)，二者永不矛盾；真实表现是“最后点击的文件未生效、展示了后到覆盖的陈旧但自洽的文件”，而非问题所述的“B 标题 + A 正文”张冠李戴。
- **影响**：预览内容与文件名张冠李戴，用户基于错误内容做判断（例如以为某配置文件是空的/是某内容）。在大文件+慢磁盘/慢网络下必现。
- **建议修法**：为 openFile/open 各引入一个 requestId（useRef 自增）或 AbortController：在 setPreview 前比对 “这次响应是否仍是最新一次点击”。最简单：闭包捕获 const path 后，在 setPreview 前 `if (latestPathRef.current !== path) return;`。open() 同理用 dir 比对。卸载时 abort。

#### P2-80 图片预览端点 /api/file 绕过了文件页自身的工作区 jail 和敏感文件denylist
- **页面/域**：文件页 Files　**维度**：安全/隐私/越权　**置信度**：medium
- **现象**：文件页对图片用 <img src=/api/file?path=...> 渲染（files.tsx:237-241, imagePaths.ts:56-58）。但 serve_file（rest.rs:2872-2925）只做 loopback 校验 + 25MB + 图片类型 sniff，完全没有 is_sensitive() denylist、也没有 workspace jail。而同一页的 /api/files/read（files.rs:247-255）两道闸都加了。
- **复现**：在浏览器/本地进程直接请求 http://127.0.0.1:7777/api/file?path=<jail外某真实png绝对路径>，返回 200 图片字节，无视 workspace_id 与 jail。
- **根因**：crates/swarmx-server/src/routes/rest.rs:2872 serve_file 既不调用 files.rs 的 is_sensitive()，也不接收/校验 workspace_id。结果：任何能构造 http 请求到 loopback 的本地进程（恶意 MCP 子进程、被注入的依赖、落地的 XSS），可用 /api/file?path=/Users/x/.ssh/id_rsa.png 之类路径绕过 read 端点的防护——虽然要求是图片扩展名+图片字节 sniff，但 .ssh 目录、jail 外目录里真实存在的图片（如某私有仓库截图、别的工作区的设计稿）都能被任意读取，突破了 files 页“工作区隔离”的安全模型。
- **影响**：文件页宣称“默认 jail 到当前工作区”，但图片这一类型的隔离形同虚设——可跨工作区/越过 jail 读取任意图片文件，违反 CLAUDE.md 列为红线的“越权访问别的 agent/space”。
- **建议修法**：让 serve_file 复用 files.rs::is_sensitive() 做硬 denylist；并接收可选 workspace_id+all，对非 all 请求套用 is_within_any(allowed_roots) jail。若产品上图片有意全盘可读，至少 is_sensitive() 必须补上，与 read 端点保持一致。

#### P2-81 workspacesLoadFailed 文案 i18n 缺 key，英文用户看到硬编码中文
- **页面/域**：文件页 Files　**维度**：一致性/i18n　**置信度**：high
- **现象**：工作区列表加载失败时，header 显示 t('files.workspacesLoadFailed', {defaultValue:'工作区加载失败,请重试'})（files.tsx:170-172）。zh.json/en.json 的 files 块里都没有 workspacesLoadFailed 这个 key，于是中英文都落到 defaultValue 的写死中文——英文界面下夹一句中文。
- **复现**：切到英文 → 让 /api/workspaces 失败（停后端）→ header 弹出中文“工作区加载失败,请重试”。
- **根因**：web/src/routes/files.tsx:170 (引用 files.workspacesLoadFailed)；缺失处 web/src/i18n/locales/zh.json 与 en.json 的 files 块（两边均无该 key）
- **影响**：英文用户在后端起不来/网络断时看到一句中文错误，破坏一致性与本地化完整性。
- **建议修法**：在 zh.json/en.json 的 files 块补 workspacesLoadFailed（中/英各一），删除组件里的写死 defaultValue 或保持但以 key 为准。

#### P2-82 fmtSize 没有 GB 档，超大文件显示成几千 M；上限处用 1<<20 而非 1<<30
- **页面/域**：文件页 Files　**维度**：功能正确性/边界态　**置信度**：high
- **现象**：fmtSize（files.tsx:21-25）只有 B/K/M 三档，没有 G。一个 3GB 文件显示为“3072.0M”而不是“3.0G”，可读性差。
- **复现**：浏览任意含 >1GB 文件的目录，大小列显示四位数 M。
- **根因**：files.tsx:21-25 缺少 `n >= 1<<30` 的 G 分支。注：这里用的是字节阈值不会触发 JS 32 位位移符号溢出（因为没用 1<<31），但缺 G 档是确凿缺陷。serve_file 允许到 25MB 图片、read 截断 512KB，但目录列表里 e.size 来自真实文件可达 GB 级（如 video/db/二进制）。
- **影响**：浏览含大文件（数据库、视频、构建产物）的目录时大小列读起来别扭，不致命但显廉价。
- **建议修法**：加 G（必要时 T）分支：`if (n >= 1<<30) return (n/(1<<30)).toFixed(1)+'G'`。注意 1<<30 仍在 32 位安全范围；若要支持 TB 用 2**40 而非位移避免溢出。

#### P2-83 “浏览整个文件系统”勾选框 className=size-8 把原生 checkbox 拉成 32px 方块，视觉怪异且无明确风险提示
- **页面/域**：文件页 Files　**维度**：一致性/可访问性/小白路径　**置信度**：medium
- **现象**：files.tsx:153-161 的原生 <input type=checkbox className='size-8'> 被强制成 32×32px 的大方块（其它地方多用 size-4），与全站交互范式不一致，看起来像渲染 bug。且这是个“解除工作区隔离、可读全盘”的高权限开关，文案只有“浏览整个文件系统”，没有任何“可能读到敏感目录”的提示。
- **复现**：打开 /files，观察 header 右侧的勾选框明显比常规大、与其它控件不齐。
- **根因**：files.tsx:160 className='size-8' 直接作用在原生 checkbox 上（原生 checkbox 不响应 width/height 的标准盒模型一致缩放，各浏览器表现不一，常出现拉伸/偏移）。label 的 min-h-8 是为点击区，但把它套到 input 本身是错的目标。
- **影响**：小白看到一个异常大的勾选框易困惑；同时一键解除安全 jail 却无风险提示，可能无意中浏览到全盘敏感区域（虽 read 端点有 denylist 兜底，但图片端点如上条不设防）。
- **建议修法**：把 size-8 从 input 移除，改用 size-4 标准尺寸；点击热区交给 label 的 padding/min-h。可在 label 加 title 或副文案提示“将解除工作区隔离”。

#### P2-84 切换“浏览整个文件系统”后用 list?.dir 重开，可能停留在 jail 外目录导致 403/空
- **页面/域**：文件页 Files　**维度**：功能正确性/错误恢复　**置信度**：medium
- **现象**：toggleBrowseAll（files.tsx:129-133）关闭 all 时 `open(list?.dir, next)`，把当前目录原样再请求一次但 all=false。若用户此前在 all=1 下浏览到 jail 外目录再关掉开关，后端会 403（jailBlocked），用户得自己手动往上爬目录才能回到工作区内，没有自动回退到工作区根。
- **复现**：选工作区→开 all→进到工作区外某目录→关 all→左栏报“超出当前工作空间范围”。
- **根因**：web/src/routes/files.tsx:132
- **影响**：关掉“浏览整个文件系统”后页面可能直接报 jailBlocked，体验上像“关了开关就坏了”，需要用户自己点 .. 爬回去。
- **建议修法**：关闭 all 时调 open(undefined, false) 回到工作区 cwd；或先判断 list.dir 是否在工作区根内，越界则回退。

#### P2-85 512KB 文本预览整串喂给 ChatMarkdown，先做正则/fence 扫描再截断，超大文本仍有卡顿与不必要计算
- **页面/域**：文件页 Files　**维度**：性能　**置信度**：medium
- **现象**：read 端点最多回 512KB 文本（files.rs:25 MAX_READ_BYTES）。预览时 fenceWrap（files.tsx:48-52）对整串 content 跑 `content.match(/`+/g)` 全量扫描求最长反引号串，再交给 ChatMarkdown；ChatMarkdown 内部 unwrapOuterMarkdownFence 又对整串跑正则，最后才在 100K 处截断（ChatMarkdown.tsx:355-367）。即对 512KB 文本做了多次全量正则后才丢弃 80%。
- **复现**：预览一个 ~500KB 的单行 minified js 或大日志，切换时明显卡一下。
- **根因**：files.tsx:268 fenceWrap 在截断前对整串扫描；ChatMarkdown 的 MAX_RENDER_CHARS 截断发生在它内部、晚于 fenceWrap，且 files 页没有先行截断。highlight.js 对接近 100K 的单代码块高亮也偏重。
- **影响**：预览大文本文件（接近 512KB 的日志/min.js/大 JSON）时主线程会有可感卡顿，且做了无用功。非崩溃级。
- **建议修法**：files 页在 fenceWrap 前先按预览所需上限（如 100K）slice content，并显示“仅预览前 N”；避免对整串做正则。或对超阈值文本直接降级为纯 <pre> 不走 markdown 高亮。

#### P2-86 filesRead 失败时把错误塞进 content 当“文件内容”渲染，假装读成功
- **页面/域**：文件页 Files　**维度**：状态诚实性/错误处理　**置信度**：high
- **现象**：openFile 的 catch（files.tsx:119-122）在读失败时 setPreview({path, binary:false, size:0, content:`(${msg})`, truncated:false})——把错误信息伪装成一个正常文件的内容，走 ChatMarkdown fence 渲染。用户看到的是一个“成功打开、内容是 (xxx error) 的文件”，而非明确的错误态。size 显示 0B 也是假的。
- **复现**：预览一个读到一半被删的文件或无权限文件 → 右栏显示标题+0B+正文一行 (read ...: permission denied)，像正常文件。
- **根因**：web/src/routes/files.tsx:121
- **影响**：读取失败（权限不足/文件消失/后端 500）被渲染成“内容为括号文字的文件”，违反“失败不许默默显示成功/正常”的红线，用户不易看出这是错误。
- **建议修法**：给 preview 增加独立 error 字段或单独的错误态分支，用红色错误样式+重试按钮渲染，而不是塞进 content。

#### P2-87 默认资源目录最终回退到裸相对路径 'spells'/'roles'/'cli-plugins'，在安装版(CWD=/)会解析成 /spells 等不存在路径
- **页面/域**：服务启动与资源解析(main + 资源目录)　**维度**：安装版现实(最高准则) / 一致性　**置信度**：medium
- **现象**：default_spells_dir()（spells.rs:362）、default_roles_dir()（roles.rs:276）、default_plugins_dir()（plugins.rs:509）在 env 变量缺失且 CARGO_MANIFEST_DIR 候选目录不存在时，最后一行返回裸相对路径 PathBuf::from("spells") / "roles" / "cli-plugins"。安装版 sidecar 的 CWD 是 /，于是这些会被当作 /spells、/roles、/cli-plugins 去 load_dir。
- **复现**：在 CWD=/ 且未设任何 SWARMX_*_DIR、CARGO_MANIFEST_DIR 指向的构建机路径不存在时调用 default_spells_dir()，返回值为相对 "spells"，被解析为 /spells。
- **根因**：三个 default_*_dir() 的兜底分支用相对路径。当前之所以不致命，纯粹是因为 main.rs:153/168 和 plugins load_layered 都先 builtin()（include_str! 编译进二进制）再 overlay 磁盘目录，且 load_dir/merge_dir 对不存在目录 warn+skip 非致命。也就是说裸相对路径这条「死路」被 builtin 层兜住了，但它本身是误导性的、且依赖 CWD——一旦哪天某处直接用 default_*_dir() 的返回值而不走 builtin overlay（例如 plugins.rs:548 测试里就直接 load_dir(&default_plugins_dir())），在 CWD=/ 的环境就会拿到 / 下的错误路径或意外读到根目录同名目录。
- **影响**：目前无用户可见故障（builtin 兜底）。但这是一个潜伏的「相对路径依赖 CWD」反模式，正是项目头号准则点名的危险来源；后续任何绕过 builtin 的新调用点都会在安装版踩坑。
- **建议修法**：兜底分支不要返回裸相对路径。要么返回 None/明确报错，要么至少在日志里 warn 一次「resource dir 未通过 env 或 manifest 解析，回退到 CWD 相对路径（安装版下不可靠）」，让发版自检能发现。更彻底的做法：既然 spells/roles/plugins 已全部 include_str! 进二进制，磁盘 overlay 应当只在 env 变量显式给出时才尝试，manifest/裸相对路径两条 dev-only 分支可以收敛掉以消除歧义。

#### P2-88 apiBase.ts 硬编码 7777，而服务端 SWARMX_PORT 可配置：两者一旦不一致，Tauri 前端连不上自己的 sidecar
- **页面/域**：服务启动与资源解析(main + 资源目录)　**维度**：一致性 / 功能正确性　**置信度**：medium
- **现象**：main.rs:279 服务端口由 SWARMX_PORT 决定（默认 7777），server_url 随之派生。但 web/src/lib/apiBase.ts:25-26 在 Tauri prod 下把 HTTP_BASE/WS_HOST 硬编码为 127.0.0.1:7777。
- **复现**：给 release sidecar 设 SWARMX_PORT=7800 启动；前端仍向 127.0.0.1:7777 发请求 → 全部失败。
- **根因**：web/src/lib/apiBase.ts:23-24（硬编码 127.0.0.1:7777 的字面量实际在第 23 行 HTTP_BASE 与第 24 行 WS_HOST，报告所写 25-26 行略有偏差）
- **影响**：当前不触发。但它把「修端口冲突」这条最该走的路堵死了：任何给 sidecar 设非 7777 端口的改动都会让前端瞎掉，且因为是硬编码、不会有编译/类型报错来提醒。
- **建议修法**：让端口成为单一事实源：Tauri 启动 sidecar 时若需要自定义端口，应通过 Tauri 命令/注入的全局变量把端口告诉前端（或干脆让前端读一个由 Rust 侧注入的 window.__SWARMX_PORT__），apiBase.ts 据此构造 URL，而不是写死 7777。

#### P2-89 「保存中…」按钮文案对英文用户硬编码显示中文
- **页面/域**：用量与成本页 Usage　**维度**：一致性 / i18n / 状态诚实性　**置信度**：high
- **现象**：切到英文界面后，点保存价目表时按钮文字变成中文「保存中…」，而非 "Saving…"。英文用户看到中英混杂。
- **复现**：设置语言为 en，进入 /usage，改一个价目数字后点 Save，观察按钮在请求期间显示「保存中…」。
- **根因**：web/src/routes/usage.tsx:488
- **影响**：英文用户在一个明显的交互态（保存进行中）看到中文，破坏专业度与一致性。
- **建议修法**：在 zh.json/en.json 的 common 下补 saving（中：保存中…/英：Saving…），并把 usage.tsx:488 改为不依赖内联中文 defaultValue。

#### P2-90 手动刷新按钮无任何反馈：不转圈、不禁用、不防重复点击，失败静默
- **页面/域**：用量与成本页 Usage　**维度**：状态诚实性 / 真实用户路径 / 错误处理　**置信度**：high
- **现象**：点右上角刷新图标后界面毫无变化——图标不旋转、按钮不置灰。后端慢时用户无法判断是否在加载，会反复点击；每次点击都发一个并发请求。请求失败时只是悄悄把底部红色错误条点亮，按钮本身没有任何「失败/重试」信号。
- **复现**：节流网络或临时停后端，进 /usage 反复点刷新图标：图标无变化、可连点、失败时按钮无提示。
- **根因**：usage.tsx:226-234 刷新按钮 onClick={() => load(false)}，load(false) 走 showSpinner=false 分支（usage.tsx:90-101），既不 setLoading 也没有独立的 refreshing 状态，按钮无 disabled、无 aria-busy。catch 里只 setErr(true)（usage.tsx:97-98），不影响按钮外观。
- **影响**：小白用户点了没反应会以为按钮坏了或数据没动；网络慢时连点制造并发请求与请求竞态（后回的覆盖先回的）。属于「死按钮观感」+ 静默失败。
- **建议修法**：加一个 refreshing 状态：点击时 setRefreshing(true)、按钮 disabled + 图标加 animate-spin（lucide RefreshCw），finally 复位；失败时给按钮一个可见的失败态或 toast，而不仅是底部错误条。

#### P2-91 轮询静默失败时仍展示旧数字，错误条与「过期数字」同屏且无过期标记
- **页面/域**：用量与成本页 Usage　**维度**：状态诚实性 / 边界态　**置信度**：medium
- **现象**：首屏加载成功后，后台 8 秒轮询若某次失败（后端重启/断网），页面顶部弹出红色「用量数据加载失败」，但下方所有统计卡、趋势图、按模型/按 agent 表仍显示上一次的旧数字，且「更新于 HH:MM:SS」停留在上次成功时间。用户容易把旧数字当成当前值。
- **复现**：进 /usage 等首屏成功，然后 kill 后端，等 8 秒轮询触发：顶部红条出现，但卡片/图表/表格仍显示旧值，「更新于」时间不变且无过期提示。
- **根因**：usage.tsx:97-98 轮询失败仅 setErr(true)，不清空也不标记 data；渲染区 usage.tsx:267 用 `data && !loading` 判定，loading 在静默轮询时始终 false，所以旧 data 继续整块渲染。updatedAt（usage.tsx:96）只在成功时更新，失败时不动——这点是对的，但页面没有把「这些数字已过期」可视化。
- **影响**：成本/用量数字在后端不可用时仍被当作有效展示，只有一条容易被忽略的红条提示。对一个强调状态诚实的项目，这是把未确认/已失效状态当确认状态展示的灰区。
- **建议修法**：轮询失败时给数据区域加「数据可能已过期，最后成功更新于 X」的轻量标记（如把 updatedAt 文案在 err 时变灰并加「· 已过期」），或在 err 且有旧 data 时给统计区降低透明度，明确区分「实时」与「陈旧」。

#### P2-92 价目表配置文件绝对路径（含用户名/家目录）被原样渲染进 UI
- **页面/域**：用量与成本页 Usage　**维度**：安全/隐私　**置信度**：medium
- **现象**：价目表区块的 meta 行与「恢复默认」确认弹窗里，直接展示完整绝对路径，如 /Users/<用户名>/.swarmx/pricing.json，暴露了操作系统用户名与家目录结构。
- **复现**：打开 /usage，看价目表标题下 meta 行；点恢复默认看确认弹窗描述，均显示完整绝对路径。
- **根因**：后端 usage.rs:399 `"path": pricing_config_path()` 返回的是绝对 PathBuf（usage.rs:168-172 用 HOME 拼接），前端 usage.tsx:452-459 pricingMeta 与 usage.tsx:190-192 confirmResetDesc 直接插值显示。
- **影响**：桌面应用本机自看自，泄露面有限，但属于不必要地把家目录绝对路径呈现给 UI（截图/录屏分享时会带出用户名）。低危。
- **建议修法**：展示时把家目录前缀折叠成 ~（后端返回时或前端渲染时替换 HOME 前缀为 ~），既更友好也不泄露用户名。

#### P2-93 趋势图与汇总用 UTC 自然日切分，本地用户「今天」的用量可能落到错误日期柱
- **页面/域**：用量与成本页 Usage　**维度**：功能正确性 / 边界态　**置信度**：medium
- **现象**：「每日趋势」按 UTC 日聚合，但 x 轴只是把 'YYYY-MM-DD' 拆成 月/日 直接显示。处于 UTC 偏移较大时区（如东八区）的用户，本地深夜/凌晨产生的用量会被算进 UTC 的前一天或后一天，与用户对「今天」的直觉不符。
- **复现**：东八区用户在本地 00:00-08:00 间产生用量，会被记入 UTC 前一天的柱，与「今天」直觉不符。
- **根因**：store.rs:775/787 `date(at/1000,'unixepoch')` 按 UTC 取日；UsageTrendChart.tsx:22-25 fmtDay 仅做字符串拆分，无时区换算。该项目 cron 页已专门处理本地时区（见提交历史），usage 这里却仍是裸 UTC。
- **影响**：非 UTC 时区用户的每日成本归属日有偏差，跨日时段尤其明显；与项目内 cron 页已建立的本地时区预期不一致。
- **建议修法**：要么后端按用户本地偏移聚合（与 cron 页一致的固定偏移方案），要么至少在图表标注「按 UTC 日」让用户知情，避免静默错位。

#### P2-94 趋势图柱子无键盘可达 / 数据点无可访问文本，tooltip 仅鼠标悬停可见
- **页面/域**：用量与成本页 Usage　**维度**：可访问性　**置信度**：medium
- **现象**：每日趋势的明细（每天 input/output/总 token）只能通过鼠标悬停 recharts Tooltip 看到。键盘用户和读屏用户无法获取逐日数据；柱子非可聚焦元素。
- **复现**：用键盘 Tab 无法聚焦任何柱子；关掉鼠标仅用读屏，逐日明细无法读出。
- **根因**：web/src/components/UsageTrendChart.tsx:27-40,70（自定义 DayTooltip 无 role="status"/aria-live，绕过了 recharts DefaultTooltipContent 的播报属性；且未传 title/desc）+ web/src/routes/usage.tsx:286-303（按日区块只渲染图表、无等价 UsageTable/visually-hidden 数据出口）。注意：键盘聚焦其实已由 recharts v3 默认 accessibilityLayer 提供（RootSurface.js:42-53 给 SVG 设 tabIndex=0/role=application，keyboardEventsMiddleware.js 处理方向键），原根因「BarChart 默认不暴露可聚焦 data point」不成立。
- **影响**：纯鼠标交互，键盘与读屏用户拿不到逐日数据，违反可访问性。
- **建议修法**：提供等价的可访问数据出口：例如图表下方附一个 visually-hidden 的逐日数据表，或给 Bar 加 role/aria-label；至少保证按日数据有非悬停的读取途径。

#### P2-95 totals.cost_usd 把已定价与未定价模型混算，headline「总成本」在部分未定价时偏低却仅以小字 hint 提示
- **页面/域**：用量与成本页 Usage　**维度**：状态诚实性 / 功能正确性　**置信度**：medium
- **现象**：当存在未识别模型时，顶部「总成本」卡片显示的金额只统计了能定价的部分（未定价模型贡献 0），而该卡片的 hint 仅从「估算值」变成「部分模型无价目，仅 token」这一行小字。用户很容易只看大号金额，误以为这就是全部花费。
- **复现**：让某 worker 跑一个不在规则也不在 LiteLLM 表里的模型，进 /usage：总成本只含可定价模型，hint 变「部分模型无价目」，但大号金额无任何不完整标记。
- **根因**：usage.rs:354-360 未定价模型 cost=0 仍累加进 t_cost，all_priced 翻 false。前端 usage.tsx:271-275 StatCard 金额用 fmtCost(data.totals.cost_usd)，仅靠 hint=`data.totals.priced ? estimated : partialPrice` 区分，金额本身不带任何「不完整」视觉信号。
- **影响**：在混用未知模型时，醒目的大号总成本是偏低的不完整值，仅靠一行容易被忽略的小字说明，存在让用户低估真实花费的诚实性风险。
- **建议修法**：未完全定价时给金额本身加视觉信号（如加「≥」前缀或问号角标），或把 partialPrice 提示提到更显眼层级，明确「这只是可定价部分的成本」。

#### P2-96 Token 预算输入无上限校验，超大数字会让后端 i64 反序列化报 500（原始错误回显给用户）
- **页面/域**：目标页 Goals　**维度**：功能正确性 / 边界态　**置信度**：medium
- **现象**：在预算框粘贴/输入超长数字（如 99999999999999999999），点创建，请求 500 失败，目标没建成，且错误条显示原始解析错误。
- **复现**：预算框输入 20 位以上的数字 → 创建 → 500。
- **根因**：web/src/routes/goals.tsx:111 (Number(rawBudget) 无 isSafeInteger/上界校验) + goals.tsx:219 (输入框不限长度)；后端实际拒绝点为 crates/swarmx-server/src/routes/goals.rs:116 的 Json<CreateGoalRequest> 提取器（返回 422 而非 500，非报告所指的 goals.rs:135）
- **影响**：属罕见输入，但会以最难懂的方式（500+原始报错）失败，且没有前端提示告诉用户『预算数值过大』。
- **建议修法**：前端对 budget 做范围/长度校验（Number.isSafeInteger 检查、限制到合理上限），超出时禁用创建或给提示，而不是把超界值发给后端。

#### P2-97 验收标准列表用「标准文本」当 React key，两条相同文案会触发 key 重复并丢失渲染
- **页面/域**：目标页 Goals　**维度**：功能正确性 / 数据完整性　**置信度**：high
- **现象**：若一个目标的验收标准里有两条完全相同的文字（用户在多行文本里重复写了同一句），列表只会稳定渲染一条，且 React 抛 duplicate key 警告。
- **复现**：新建目标，验收标准框输入两行一模一样的文字 → 创建 → 卡片只显示一条 + 控制台 key 警告。
- **根因**：goals.tsx:354-362 `goal.success_criteria.map((c) => <li key={c}>`，用字符串内容当 key。parseCriteria（goals.tsx:38-43）只 trim+去前缀+去空，不去重，所以重复内容能进数组；渲染时相同 key 冲突。
- **影响**：边界场景下验收标准显示缺条，用户以为某条标准没保存上。属数据呈现不忠实。
- **建议修法**：key 改用稳定索引或 `${i}-${c}`；或在 parseCriteria 里去重（去重更符合语义，但要确认产品意图）。

#### P2-98 「刷新」按钮在加载中无 disabled / 无转圈，可被狂点发起并发重复请求
- **页面/域**：目标页 Goals　**维度**：性能 / 防重复点击 / 一致性　**置信度**：medium
- **现象**：右上角刷新按钮（RefreshCw 图标）正在加载时图标不动、按钮可继续点；连点会并发多个 listGoals 请求，最后回来的覆盖先回来的，可能出现状态闪烁。
- **复现**：在目标较多的工作区快速连点刷新按钮，观察按钮无 disabled 反馈、网络面板出现多个并发 /api/goals。
- **根因**：goals.tsx:153-161 的刷新按钮只有 onClick={load}，没有 disabled={loading}，图标也不随 loading 旋转（对比下方 create/addEvidence 按钮都有 Loader2 动画与 disabled，goals.tsx:228-233、411-417）。listGoals 也没走 dedupe（http.ts 里只有 plugins 用了 dedupe，http.ts:151）。这与同页其它按钮的交互范式不一致。
- **影响**：并发请求 + 末位覆盖可导致短暂状态错乱；体验上按钮无反馈让人误以为没点到。非致命但属一致性/性能问题。
- **建议修法**：刷新按钮加 disabled={loading} 并在 loading 时给 RefreshCw 加 animate-spin；可选地给 listGoals 做请求去重或丢弃过期响应（请求序号/AbortController）。

#### P2-99 证据类型(kind)输入框无 placeholder、语义不明，小白不知道该填什么
- **页面/域**：目标页 Goals　**维度**：真实用户路径（小白） / 一致性　**置信度**：low
- **现象**：展开证据区后，第一个窄输入框（120px）初始填着 `note` 但无 placeholder、无任何说明；若用户清空它再填摘要，提交时虽有兜底但 UI 没解释这个框是干嘛的。
- **复现**：展开任一目标的『证据』，观察第一个窄框无 placeholder、语义不明。
- **根因**：goals.tsx:396-401 的 evidenceKind 输入只有 aria-label，无 placeholder、无候选项提示（做成了自由文本框，初值硬编码 'note'，goals.tsx:285）。addEvidence 里 goals.tsx:319 用 `evidenceKind.trim() || 'note'` 兜底，后端 goals.rs:263-271 要求 kind 非空——前端兜底掩盖了这一点，但用户得不到任何关于该填什么的引导。文案上『证据/证据类型/证据摘要』对首次使用者也偏抽象。
- **影响**：小白不理解『证据类型』要填什么，自由文本还会产生五花八门的脏分类，削弱后续按 kind 聚合的价值。属可用性问题，非崩溃。
- **建议修法**：把 kind 改成下拉（预置 note/proof/blocker 等枚举）或至少给 placeholder + 说明；摘要框已有 placeholder 可保留。

#### P2-100 同一行内工作区选择器用 Radix Select、方向选择器用原生 <select>，交互范式不统一
- **页面/域**：目标页 Goals　**维度**：一致性 / 可访问性　**置信度**：medium
- **现象**：头部右上角，工作区下拉是自定义 Radix Select（有自定义样式/键盘行为），紧挨着的方向下拉是浏览器原生 <select>，两者外观、键盘交互、弹层行为都不一致。
- **复现**：肉眼对比 /goals 头部两个下拉的样式与点开行为。
- **根因**：web/src/routes/goals.tsx:162 (WorkspacePicker，Radix) 与 web/src/routes/goals.tsx:163-177 (原生 <select>) 相邻混用两套下拉实现；WorkspacePicker 内部见 web/src/components/WorkspacePicker.tsx:39-81，Radix 来源见 web/src/components/ui/select.tsx:4
- **影响**：视觉与交互不一致（尤其键盘/焦点表现不同），属打磨问题；原生 select 虽可访问但与全站 Radix 风格割裂。
- **建议修法**：把方向选择器也换成统一的 Select 组件（或反之），保证同排控件范式一致。

#### P2-101 每次切换工作区都强制重新『确认连接』,持久 shell 体验被打断
- **页面/域**：终端页 Terminal　**维度**：一致性 / 真实用户路径　**置信度**：medium
- **现象**：已经连上 A 工作区的终端,切到 B 工作区时,armed 被重置回 false(terminal.tsx:53-55),整个 xterm 视图被卸载,重新显示『连接本机终端』确认卡片,必须再点一次『连接终端』。即便切回 A 也要重新确认。
- **复现**：连上 A → 切到 B → 看到确认卡片需再次点连接 → 切回 A → 又要再点一次。
- **根因**：web/src/routes/terminal.tsx:53-55
- **影响**：页面宣称每个工作区有独立持久 shell,但 UX 上每次切换都像第一次进入,持久性的价值被确认门禁稀释;频繁切换工作区的用户会觉得啰嗦、不一致(其他工具页切工作区不会弹门禁)。
- **建议修法**：考虑把『已确认/已 armed』提升为一次性的会话级状态(例如 sessionStorage 记一个 armed 标记),切工作区只重建连接、不重新弹确认;或者干脆首次确认后整页会话内不再询问。

#### P2-102 终端容器与失败态缺可访问性:无 aria-label,断线信息只藏在 xterm 文本里
- **页面/域**：终端页 Terminal　**维度**：可访问性　**置信度**：medium
- **现象**：终端宿主 div(terminal.tsx:131)只是个裸 div ref,没有 role/aria-label/区域名,屏幕阅读器读不出『这是终端区域』。连接断开的唯一信号 [session closed] 是直接 write 进 xterm canvas 的转义文本,辅助技术与 React 层都拿不到这个状态。
- **复现**：用读屏(VoiceOver)进入 /terminal,焦点落到终端区时无名称播报;断线时 [session closed] 不会被播报。
- **根因**：terminal.tsx:131 的 host div 无任何 aria 属性;失败/关闭状态(terminal.tsx:99)只写入 xterm,没有任何可被 a11y 树或 React 状态感知的等价文本(如 role=status 的 live region)。WorkspacePicker 的 trigger 倒是有 aria-label(WorkspacePicker.tsx:56),所以问题集中在终端区本身。
- **影响**：键盘/读屏用户无法得知终端区域存在与其连接状态;断线这种关键状态对辅助技术完全不可见。
- **建议修法**：给 host 容器加 role 与 aria-label(如 aria-label=终端);把连接/断开状态额外渲染成一个 role=status 的可见或视觉隐藏 live region,让状态变化可被读屏播报,而不是只存在于 xterm canvas。

#### P2-103 「模型」页切换分区会静默丢弃未保存的编辑，无任何提醒
- **页面/域**：设置页 Settings　**维度**：数据完整性 / 真实用户路径　**置信度**：high
- **现象**：用户在 设置→模型 里把各 CLI 的 default / opus-sonnet-haiku / effort 改了一堆，但没点「保存」，随手点左侧别的分区（或按 Cmd+1~7 快捷键）切走，再切回来——所有改动消失、回到服务器旧值，没有任何「有未保存更改」的提示或拦截。
- **复现**：模型页改任一输入框→不点保存→点左侧「通用」→再回「模型」：改动丢失，无提示。
- **根因**：settings.tsx:217-229 各 Panel 是条件渲染，切分区即卸载 ModelsPanel，其本地 cfg state（444）随之销毁；只有点 save→putModels 才落库。期间无 dirty 追踪、无离开确认、无 beforeunload。Cmd+1~7 的 keydown 处理器(144-155)也不判断焦点是否在输入框，进一步增加误触切走的概率。
- **影响**：辛苦填的模型映射一键蒸发，且毫无征兆。对认真配置模型的用户是实打实的数据丢失体验。
- **建议修法**：给 ModelsPanel 加 dirty 判断（cfg 与 data.config 不等时）：切分区/关页前用 ConfirmActionDialog 提示「有未保存更改」，或在面板顶部常驻一条「未保存」横幅。section-switch 快捷键(144-155)在 e.target 为 input/textarea 或处于 contentEditable 时应跳过。

#### P2-104 分区切换快捷键 Cmd/Ctrl+1~7 在输入框聚焦时不被拦截，可能打断模型 id 输入并切走
- **页面/域**：设置页 Settings　**维度**：可访问性 / 一致性　**置信度**：medium
- **现象**：在「模型」页的模型 id 输入框里，用户若按 Cmd/Ctrl+数字（部分用户用它做编辑/选词习惯，或误触），会被全局监听捕获并 preventDefault + 跳到对应分区，丢掉正在输入的内容（叠加上面的未保存丢失问题）。
- **复现**：模型页输入框聚焦→按 Cmd+2：直接跳到「外观」分区，输入中断。代码 144-155 无焦点守卫。
- **根因**：settings.tsx:144-155 的 keydown 监听只判断 metaKey/ctrlKey + 数字范围，不判断 e.target 是否为可编辑元素，window 级监听对输入框一视同仁。
- **影响**：输入态被快捷键抢走，体验割裂；与多数应用「编辑框内禁用导航快捷键」的范式不一致。
- **建议修法**：在 onKey 开头加守卫：const el = e.target as HTMLElement; if (el && (el.tagName==='INPUT'||el.tagName==='TEXTAREA'||el.isContentEditable)) return;

#### P2-105 「保存」永远在 200 后弹「已保存」，即使没有任何改动也提示成功（轻微）
- **页面/域**：设置页 Settings　**维度**：状态诚实性　**置信度**：low
- **现象**：模型页即便一字未改点保存，也会请求 PUT 并弹『已保存』2.5s。算不上谎言（确实 PUT 成功了），但对用户而言「我没改也说保存了」略微稀释了反馈的信息量。
- **复现**：模型页不改任何东西直接点保存 → 弹「已保存」。逻辑见 497-512。
- **根因**：ModelsPanel.save (settings.tsx:497-512) 无 dirty 判断，无条件 putModels；服务器 echo 回 config 即视为 saved=true。
- **影响**：影响很小，仅反馈语义略弱 + 一次无谓的写盘（后端会原子写 ~/.swarmx/models.json）。
- **建议修法**：可选：cfg 与已知 data.config 相等时禁用「保存」按钮或跳过请求；非阻塞性优化。

#### P2-106 × 终止按钮在后端失败时仍乐观移除 agent，可能「假终止」+ 被 WS 刷新复活
- **页面/域**：调试页 Debug　**维度**：状态诚实性 / 数据完整性 / 竞态　**置信度**：high
- **现象**：点 × 终止：即使后端 killAgent 抛错（agent 没真被杀），前端也立刻把这个 pane 从列表里抹掉，用户以为已终止；但底层进程可能还活着，且随后一次 WS 触发的 refreshAgents 会把它重新加回来，pane 闪一下又出现。
- **复现**：在 kill 接口返回非 2xx 的情况下点 × —— pane 立即消失（无任何错误提示），随后任意 agent_state 事件触发刷新后该 pane 重新出现。
- **根因**：web/src/routes/debug.tsx:139-154 (kill 的 catch 仅 console.warn 后无条件乐观移除、对传输层失败也不提示)；后端实际路径为 crates/swarmx-server/src/routes/rest.rs:1063-1113 (teardown_agent 一旦命中 registry 即返回 204，record_agent_kill DB 写失败被 tracing::warn! 吞)，而非报告所称的「teardown 失败返回非 2xx」
- **影响**：用户看到「已终止」是假象（界面撒谎），真实进程仍在跑、仍在烧 token；或者抹掉后又复活造成视觉错乱。属于状态诚实性 + 数据完整性双重问题。
- **建议修法**：把乐观移除挪到 try 成功之后；catch 里不移除、改为弹错误提示让用户知道终止失败可重试。或保留乐观 UI 但失败时回滚（把 agent 加回 + 提示）。

#### P2-107 spawn 乐观插入与 WS 触发的 refreshAgents 竞态，会产生重复 agent / React key 冲突
- **页面/域**：调试页 Debug　**维度**：功能正确性 / 竞态 / 性能　**置信度**：medium
- **现象**：点 + Claude 启动 agent 时，偶发同一个 agent 出现两个 pane（重复渲染），并在控制台报 React duplicate key 警告，重复 pane 会各开一条 PTY WebSocket。
- **复现**：在网络/事件时序下（WS spawning 事件 + 200ms 刷新落在 await spawnAgent resolve 之前）连续点 + Claude，观察是否出现重复 pane 与 console 的 "Encountered two children with the same key" 警告。
- **根因**：web/src/routes/debug.tsx:125-137 spawn() 成功后 setAgents(prev=>[...prev, agent]) 无去重直接 append。与此并行，spawn 本身会触发后端广播 agent_state=spawning，useSwarmFeed 收到后 scheduleRefresh()（debug.tsx:98-105）在 200ms 后调用 refreshAgents()，后者用后端真实列表整体 setAgents 替换（debug.tsx:64-71）。若刷新先于乐观 append 落地、且刷新结果已含新 agent，则 append 会再加一份同 agent_id，导致列表里两条相同 agent_id（304 行 .map 用 agent.agent_id 作 key）。
- **影响**：重复 pane = 重复 XtermPane 挂载 = 同一 agent 开两条 /ws/pty WebSocket + 两个 xterm 实例（webglPool 名额浪费），并触发 React 同 key 渲染告警；交互上两个 pane 操作互相打架。虽是 debug 页影响面有限，但属真实竞态 bug。
- **建议修法**：spawn 成功的乐观插入做去重：setAgents(prev => prev.some(a=>a.agent_id===agent.agent_id) ? prev : [...prev, agent])；或干脆只依赖 refreshAgents、去掉乐观 append。

#### P2-108 图标按钮（⚡ _ □ ×）无 aria-label，屏幕阅读器读出无意义符号；点击区域/对比度偏弱
- **页面/域**：调试页 Debug　**维度**：可访问性　**置信度**：high
- **现象**：agent 头部的 ⚡ / _ / □ / ❐ / × 按钮只有视觉符号 + title 属性，没有可访问名称；用键盘+读屏访问时读出的是「闪电」「下划线」「乘号」之类，无法理解功能。
- **复现**：用键盘 Tab 到 agent 头部按钮 + 打开 VoiceOver/NVDA，听到读出的是符号名而非动作名。
- **根因**：debug.tsx:339-360 这组按钮只设了 title（鼠标 hover 提示，读屏支持不稳定），没有 aria-label；按钮内文本是单个 emoji/符号。顶部 dock 还原按钮（387-403）、显示/隐藏面板按钮同理。多处用 color:#64748b 的浅灰小字（12px）在 #111827 上对比度偏低。
- **影响**：纯键盘 + 读屏用户无法分辨这几个操作（唤醒/最小化/最大化/终止都是单字符），误触即可能终止 agent。虽是内部调试页，但仍违反可访问性维度。
- **建议修法**：给每个图标按钮加 aria-label（如 aria-label="手动唤醒" / "最小化" / "最大化" / "终止 agent"）；title 可保留。提升浅灰说明文字的对比度或字号。

#### P2-109 插件按钮无视后端 installed 标志，点未安装的 CLI 用阻塞 alert 报错
- **页面/域**：调试页 Debug　**维度**：真实用户路径 / 一致性 / 错误处理　**置信度**：high
- **现象**：顶栏为每个 CLI 渲染一个 + 按钮，包括服务端探测到未安装的 CLI。点未安装的（如 + Codex）会等 spawn 失败后弹一个浏览器原生 alert，文案是后端原始英文报错。
- **复现**：在没装 codex 的机器上进 /debug，点 + Codex，观察阻塞式原生 alert 弹出。
- **根因**：crates/swarmx-server/src/routes/rest.rs:213-218（后端 installed 标志，审查员写的 rest.rs 行号正确，补全完整路径）；前端缺陷在 web/src/routes/debug.tsx:243-252（按钮无视 installed）与 web/src/routes/debug.tsx:131-133（阻塞式 alert）
- **影响**：用户被引导去点一个注定失败的按钮，再被一个阻塞式英文 alert 打断，自助恢复成本高、与产品其余部分交互范式割裂。
- **建议修法**：对 installed===false 的插件按钮加 disabled + title 提示「未检测到该 CLI」（或直接不渲染）；把 alert 换成 AppToaster 错误 toast。

#### P2-110 ensureDebugWorkspace 依赖永不存在的 __SWARMX_HOME，cwd 恒为 /tmp，且 listWorkspaces dedupe TTL=0 留竞态窗口建出重复 debug-scratch
- **页面/域**：调试页 Debug　**维度**：功能正确性 / 数据完整性　**置信度**：medium
- **现象**：Debug 页首次 spawn 时自动建的 debug-scratch 工作区，cwd 永远是 /tmp（即便用户机有合理项目目录）；并发首点多个 + 按钮时可能建出多个同名 debug-scratch 工作区。
- **复现**：代码审计：grep __SWARMX_HOME 仅 debug.tsx 命中、无写入处 → cwd 必为 /tmp。并发 spawn 时序下两次 listWorkspaces 都早于第一次 createWorkspace 完成即重复建。
- **根因**：web/src/routes/debug.tsx:111-123
- **影响**：agent 永远在 /tmp 下跑（与用户预期项目目录无关，文件落 /tmp 易被系统清理 → 数据完整性风险）；并发点击产生孤儿重复工作区。仅影响 debug 页，但属真实逻辑缺陷。
- **建议修法**：去掉对 __SWARMX_HOME 的依赖（要么真的注入它，要么改用后端已知的默认 cwd）；ensureDebugWorkspace 加单飞（用一个 module 级 Promise 缓存）避免并发重复建。

#### P2-111 整个 Ledger 页面英文用户看到的全是中文（ledger.* i18n key 在两个语言包里都不存在）
- **页面/域**：账本 Ledger 视图　**维度**：一致性 / 真实用户路径（i18n 漏翻）　**置信度**：high
- **现象**：把语言切到 English，Ledger 页标题、副标题、两张卡片标题/副标题/空状态、压缩/刷新按钮、压缩结果提示——全部仍是中文（'AI 工作台账'、'任务台账'、'压缩'、'刷新'、'已压缩，省约 N tokens'…）。
- **复现**：设置里切到 English → 进任意空间 Ledger 标签 → 顶栏与卡片全是中文。
- **根因**：web/src/i18n/index.ts:39（缺失 key 回退返回中文 defaultValue）；触发点 web/src/routes/workspace/views/Ledger.tsx:201,202,241,244,256,260,270,280,281,291,298；缺失字典文件 web/src/i18n/locales/zh.json 与 en.json（均无顶层 ledger 命名空间）
- **影响**：英文用户（fallbackLng 之外的真实用户）整页中文，等于没做国际化。也违反 i18n/index.ts 注释里'编辑 zh.json+en.json 才算完成'的约定——这些 key 从未落地词典。
- **建议修法**：把 Ledger 用到的所有 ledger.* key 补进 zh.json 和 en.json（en 给英文译文）。或至少补 en.json。

#### P2-112 压缩双台账时一边成功一边失败，会被当成完全成功上报
- **页面/域**：账本 Ledger 视图　**维度**：状态诚实性 / 错误处理　**置信度**：high
- **现象**：若 task.ledger 压缩成功（省了 N tokens）而 progress.ledger 压缩请求失败（网络/超时/500），界面显示「已压缩，省约 N tokens」，完全不提示 progress 那一边失败了。
- **复现**：较难自然触发；可在两次 compact 调用中让 progressKey 那次返回 5xx，观察仍显示成功文案。
- **根因**：Ledger.tsx:191-197：两个 compact 各自 `.catch(() => null)`，再 `results.filter(r => r && r.changed).reduce(...省 tokens)`。失败那条变 null 被 filter 掉，只要任一条成功 saved>0 就报成功，partial failure 被掩盖。
- **影响**：用户以为两份台账都压缩了，实际只压了一份；与红线'不许把失败默默吞掉显示成功'相抵触（程度比 P0 那条轻，因为至少有一份真成功）。
- **建议修法**：分别判定两条结果：任一条是错误就在提示里点明哪份失败、为什么（沿用上面 P0 的三态提示模型），而不是只看 saved 之和。

#### P2-113 每秒整页重渲染导致 ReactMarkdown 对大台账每秒重新解析一次
- **页面/域**：账本 Ledger 视图　**维度**：性能　**置信度**：medium
- **现象**：台账内容很长（Magentic-One 台账会无界增长，正是'压缩'要解决的场景）时，页面持续轻微卡顿/CPU 偏高，即使没有任何 blackboard 事件。
- **复现**：造一个数千行的 task.ledger.md，打开 Ledger，用 React Profiler 观察每秒一次包含 ReactMarkdown 的 commit。
- **根因**：Ledger.tsx:82-85 每 1000ms `setNowTick(Date.now())` 触发 LedgerView 重渲染（为了让'XX 秒前'跳动）。LedgerCard（369 行）和 BreadcrumbsCard（315 行）都是普通函数组件、未 React.memo，且没把 nowTick 与卡片渲染解耦——于是每秒都重渲染两张 LedgerCard，内部 `<ReactMarkdown remarkPlugins={[remarkGfm]}>` 对整份台账 Markdown 重新解析+重建 AST，1 次/秒、60 次/分。
- **影响**：台账越大越明显的持续 CPU 开销；纯粹为了刷新一个'秒数'文本而每秒重解析整页 Markdown，浪费且可能掉帧。
- **建议修法**：用 React.memo 包 LedgerCard 并把 stripLedgerHeading+ReactMarkdown 的产物用 useMemo 按 snap.content 缓存；或把'XX 秒前'的 tick 下沉到独立小组件，避免每秒重渲染携带 Markdown 的卡片。

#### P2-114 「刷新」按钮在压缩进行中未禁用，可并发触发重复 listBlackboard
- **页面/域**：账本 Ledger 视图　**维度**：防重复点击 / 一致性　**置信度**：medium
- **现象**：点了「压缩」后（最长可达 90s，因为后端跑 claude -p），「刷新」按钮仍可点，连点会并发打多次 listBlackboard + readBlackboard。
- **复现**：点'压缩'后在转圈期间反复点'刷新'，Network 面板可见多发的 /api/blackboard 请求。
- **根因**：web/src/routes/workspace/views/Ledger.tsx:266
- **影响**：压缩长达 90s 的窗口里用户可疯狂点刷新，产生重复请求（虽有 mountedRef 防卸载后 setState，但无 in-flight 去重）；交互一致性也不对称。
- **建议修法**：刷新按钮改 `disabled={refreshing || compacting}`，与压缩按钮的互斥条件对齐。

#### P2-115 读取台账失败时只在卡片内显示开发者向英文技术串，无重试入口，小白难自助恢复
- **页面/域**：账本 Ledger 视图　**维度**：空/加载/错误态 / 失败恢复　**置信度**：medium
- **现象**：后端没起或断网时，卡片里显示'读取失败: GET /api/blackboard → 500: …'这种英文 dev 文案，小白看不懂；且没有'重试'入口，只能再去点顶栏刷新。
- **复现**：停掉后端再打开 Ledger，卡片显示 'GET /api/blackboard → ...' 之类英文技术串。
- **根因**：Ledger.tsx:401-404 直接渲染 `snap.error`，而 snap.error 来源是 (e as Error).message（loadOne:120 / refresh:168），对 ApiError 来说是 `METHOD path → status: detail` 的 dev 文案（见 http.ts:97）。没有把 ApiError.detail 拆出来做用户向文案。404 空态由 loadOne 用 listBlackboard 预判规避了，主要问题是 5xx/断网时的文案与恢复引导。
- **影响**：错误态展示的是技术字符串而非人话；用户除了'再点刷新'没有明确恢复指引（程度中等因为刷新本身可恢复）。
- **建议修法**：渲染错误时用 (e as ApiError).detail 或一句人话'读取台账失败，请检查后端是否在运行'，并在卡片内放一个'重试'按钮直接调 refresh()。

#### P2-116 副标题把内部接口 `/ws/swarm` 直接印给终端用户看
- **页面/域**：通知中心 + 通知弹窗　**维度**：真实用户路径(小白) / 一致性　**置信度**：high
- **现象**：通知中心顶部副标题渲染为「来自 /ws/swarm · N 条未读」(zh) / 「from /ws/swarm · N unread」(en)。小白用户看到 `/ws/swarm` 这种 WebSocket 路径完全不知道是什么,像 bug/乱码。
- **复现**：—
- **根因**：web/src/i18n/locales/en.json:592 与 web/src/i18n/locales/zh.json:592（经 web/src/routes/notifications.tsx:436 渲染）。范围修正：仅 /notifications 整页 header 副标题，通知弹窗 NotificationPopover 不受影响。
- **影响**：首屏第一眼就暴露技术内幕,降低产品观感;对完全不懂技术的用户造成困惑甚至以为出错。属于「文案给错受众」的一致性问题。
- **建议修法**：把 subtitle 译文改成用户能懂的话,例如 zh「{{count}} 条未读」/ en「{{count}} unread」,去掉 `/ws/swarm` 这种实现细节。en.json:592、zh.json:592 各改一处即可。

#### P2-117 agent_state 通知标题用原始短 id + 原始英文状态,绕过 friendlyAgent 和 i18n
- **页面/域**：通知中心 + 通知弹窗　**维度**：一致性 / i18n / 真实用户路径(小白)　**置信度**：high
- **现象**：实时收到 agent 状态变化时,通知标题渲染成类似「codex-6fc9b645 → exited」这种原始 agent 短 id + 写死英文状态词,而同一列表里其它所有通知(消息/共享区/唤醒)都是友好角色名 +中文。中英混杂、术语过载。
- **复现**：—
- **根因**：web/src/routes/notifications.tsx:357-358
- **影响**：小白看到一串 hash 短 id 和英文状态词不知所云,且与列表其它条目风格割裂。在中文 locale 下尤其突兀。
- **建议修法**：title 改为 `${friendlyAgent(ev.agent_id, roleRef.current, t)} → ${t('notifications.state.'+ev.state)}`,并为每个 SwarmAgentState 加一组译文 key(zh/en);agent 字段也用 friendlyAgent 包一层。注意:useNotificationBadge 不为 agent_state 点亮红点(useNotificationBadge.ts:59-61),所以这类通知只在 popover/页面已打开实时滚入时出现,但出现时就该是友好文案。

#### P2-118 blackboard 通知的已读状态依赖 `at` 时间戳做 id,刷新后大概率丢失
- **页面/域**：通知中心 + 通知弹窗　**维度**：数据完整性 / 一致性　**置信度**：medium
- **现象**：用户在通知中心把一条共享区(blackboard)通知标为已读,刷新页面后它很可能又变回未读。
- **复现**：—
- **根因**：web/src/routes/notifications.tsx:294 (已读集合用易变 id);配合 notifications.tsx:243-247 与 381-382 (仅 msg- 同步服务端、bb- 全靠 localStorage);crates/swarmx-storage/src/store.rs:1916 (list_blackboard_ops(None) 取 MAX(id) 使 at 随每次写入而变)。popover NotificationPopover.tsx:152 仅用于 live 去重，非症状落点。
- **影响**：共享区类通知的已读状态在最常见场景(台账被反复更新)下不稳定,用户反复清同一条,体验上像「标不掉」。是数据完整性/一致性问题。
- **建议修法**：blackboard 通知 id 改用稳定标识(如 `bb-${path}` 或服务端 op `id`,BlackboardEntry 暂无 id 字段——可让 list_blackboard_paths 带上 op id,types.ts:230 的 BlackboardEntry 加 id),把已读绑到稳定键而非易变的 at。

#### P2-119 「全部刷新」按钮无 loading / 防重复点击,标题栏刷新与列表无任何加载态
- **页面/域**：通知中心 + 通知弹窗　**维度**：状态诚实性 / 性能 / 防重复点击　**置信度**：medium
- **现象**：通知中心右上刷新按钮(RefreshCw)和首屏加载期间,界面没有任何 loading 指示;连点刷新会并发触发多次 `Promise.all`(4 个请求 ×N),且最后回来的那次覆盖前面的,可能出现旧数据盖新数据的竞态。
- **复现**：—
- **根因**：web/src/routes/notifications.tsx:451（刷新按钮 onClick={seed}，无 disabled/spinner）+ web/src/routes/notifications.tsx:266-307（seed 无 in-flight 守卫/无 loading 态）+ web/src/routes/notifications.tsx:303（setItems(all) 整数组替换，为 race 覆盖点）
- **影响**：用户狂点刷新→请求风暴 + 可能短暂显示过期列表;且全程不知道在不在加载(诚实性:转圈都没有,用户不知点了有没有用)。数据量大时 4 路全量拉取也偏重。
- **建议修法**：给 seed 加 `loading` state:进行中按钮 disabled + RefreshCw 转圈,完成才放开;用一个 in-flight ref 或 AbortController 取消上一次,避免 race 覆盖。

#### P2-120 popover 打开即 onSeen() 清红点,但不标任何消息已读 → 与 /chat、/notifications 未读数不一致
- **页面/域**：通知中心 + 通知弹窗　**维度**：一致性 / 状态诚实性　**置信度**：medium
- **现象**：点铃铛打开弹窗,右上红点立刻消失(标 seen),但弹窗里的消息一条没真正标已读;关掉弹窗去 /notifications 或 /chat,这些消息仍然全是未读、计数还在。用户以为「看过了红点没了」,可未读数却还在别处亮着,自相矛盾。
- **复现**：—
- **根因**：NotificationPopover.tsx:169-173 `useEffect(open)` 里只调 `onSeen()`(= useNotificationBadge.markSeen,仅写 seenAt 时间戳,useNotificationBadge.ts:72-80),从不调用 api.markMessagesRead,也不写 READ_KEY。而 /notifications 用独立的 READ_KEY 集合 + 服务端 read_at 计数。两套已读体系不联动:popover 的 seen 只压红点,不影响 per-item 未读。
- **影响**：三处未读口径(铃铛红点 / chat badge / 通知中心 per-item)各算各的,用户看到「红点没了但数字还在」的矛盾。属一致性瑕疵,非数据损坏,故 P2。
- **建议修法**：明确产品语义:若 popover 是「瞄一眼不算读」则可接受现状,但应保证铃铛红点逻辑与通知中心未读至少方向一致(例如红点基于 totalUnread 而非单独 seenAt);若希望打开即读,则在 refresh 后对展示的 message 项批量 markMessagesRead。二选一,避免现在的半联动矛盾。

#### P2-121 弹窗与通知中心列表均无虚拟化,且每条 li 在 render 内重建 kindBg/kindIcon 对象
- **页面/域**：通知中心 + 通知弹窗　**维度**：性能 / 重渲染　**置信度**：low
- **现象**：通知中心列表上限 200 条(notifications.tsx:365 slice(0,200)),全量渲染无虚拟化;每条 li 渲染时都在 map 回调里重新构造 `kindBg`、`kindIcon` 两个 Record 对象(504-517),200 条 ×2 对象 ×每次重渲染。实时事件高频到达时整个 ul 频繁重渲。
- **复现**：—
- **根因**：web/src/routes/notifications.tsx:504-517（kindBg/kindIcon 在 filtered.map 回调内每条每次渲染重建，应提为模块级常量）+ web/src/routes/notifications.tsx:501-576（列表无虚拟化、行无 React.memo）。注意：标题所指的"弹窗"NotificationPopover.tsx 不适用——它硬限 12 条（第57行 MAX_ITEMS=12）、图标映射已 useMemo 提升（第228-235行）、无 kindBg。
- **影响**：长时间运行、消息密集的 swarm 下,通知列表会有可感卡顿与不必要 GC;200 条上限缓解了上界但中等量已可见。属性能优化,非崩溃。
- **建议修法**：把 kindBg/kindIcon 提为模块级常量;列表条目用 React.memo;条目数大时引入虚拟化(与 MessagesPanel 同方案)。

#### P2-122 通知列表项整卡片不可键盘聚焦/激活,只有 X 按钮可达;弹窗条目无 aria 角色描述
- **页面/域**：通知中心 + 通知弹窗　**维度**：可访问性　**置信度**：medium
- **现象**：通知中心每条通知是 li(非按钮),整卡片不可 Tab 聚焦、不可回车;键盘用户只能 Tab 到右上的 X(标已读)按钮,无法用键盘「打开/跳转」该通知(实际上列表项本身也确实没有点击跳转——见下)。弹窗里条目是 button 可聚焦,但没有 aria-label 概括「未读/类型/时间」,读屏只能逐段念。
- **复现**：—
- **根因**：notifications.tsx:520 li 无 tabIndex/role,卡片本身无 onClick(只有 X 有 onClick=markRead);相比之下 popover 的条目是 button。两个面的交互范式不一致(通知中心卡片不可点跳转,弹窗卡片可点跳转)。a11y 上通知中心仅 X 可键盘达。
- **影响**：读屏/纯键盘用户在通知中心难以高效操作;且通知中心卡片不可点跳转、弹窗卡片可点跳转,交互不一致让用户困惑「为什么列表里点了没反应」。
- **建议修法**：统一交互:让通知中心卡片也可点击跳到对应 workspace/会话(与 popover 一致),并用 button/role+tabIndex 暴露;给条目加 aria-label 概括类型/未读/时间。

#### P2-123 classifyMessage 用关键词正则给 agent 自由文本判「异常/完成」,中文叙述下仍有可观误判面
- **页面/域**：通知中心 + 通知弹窗　**维度**：功能正确性 / 状态诚实性　**置信度**：medium
- **现象**：agent 总结消息被正则归类为 error/completed,会出现把正常报告标红「异常」或把含失败描述的标绿「完成」的情况。代码注释自己也承认规则 4 里「没有 traceback」这类否定式仍可能误报为 error。
- **复现**：—
- **根因**：web/src/routes/notifications.tsx:160-228 (classifyMessage 正则分级；红色告警样式在 web/src/routes/notifications.tsx:508)
- **影响**：误把完成报告标成红色异常 = 让做完的任务看起来坏了(项目里被点名为「最糟的假阳性」);反向则把失败标成完成,瞒报问题。虽已多轮加固,但启发式本质决定了仍有误判,影响状态诚实性。confidence 给 medium 是因为已知会误判但触发取决于 agent 措辞。
- **建议修法**：长期方向:让后端在消息 meta 里 stamp 结构化 subtype(completion 已有 meta.subtype,见 134),error/completed 都走 meta 而非 regex prose;前端 regex 仅作 meta 缺失时的兜底,并在 UI 上对兜底分类弱化告警色(避免误判直接红色告警)。

#### P2-124 Home 自动重定向逻辑：刚加载就跳进第一个工作空间，列表/欢迎屏几乎永远见不到
- **页面/域**：首页/工作空间列表 + 新建空间向导　**维度**：一致性 / 真实用户路径　**置信度**：medium
- **现象**：只要存在任意一个工作空间，进 /chat 会立即 navigate 到 /chat/<第一个>，用户永远停不在‘工作空间列表+欢迎屏’这个页面；多工作空间用户想回到总览也回不来(一进就被弹走)。
- **复现**：—
- **根因**：web/src/routes/chat/Home.tsx:124-128
- **影响**：并非崩溃，但产品上‘工作空间总览’不可达，和侧栏‘回到列表’这类心智预期不一致；也让本页 Welcome 分支(line 200)在非空场景永不展示。
- **建议修法**：确认这是产品有意为之就保留，但建议：仅当用户是‘首次/刚 spawn’场景才自动跳，或保留一个不会被自动弹走的总览入口；否则这段重定向应配文档说明，避免被当成 bug 反复改。

#### P2-125 向导路径校验的‘目录不存在’中文化只覆盖部分服务端文案，文件/不可读路径回退英文原文
- **页面/域**：首页/工作空间列表 + 新建空间向导　**维度**：一致性 / i18n / 错误处理　**置信度**：medium
- **现象**：用户在主项目路径填了一个‘存在但是文件、不是目录’的路径，或填了无权限目录时，红字报错是英文服务端原文(如 `not a directory: /x` / `read_dir /x: Permission denied`)，与其它中文报错混杂。
- **复现**：新建向导→主项目路径填一个真实文件路径(如 /etc/hosts)→等校验→红字显示英文 ‘not a directory: /etc/hosts’。
- **根因**：CreateWizard.tsx validatePath 的正则只匹配 `/directory does not exist|not found|no such/i`(line 245) 来翻成中文；但 files.rs list_dir 对‘是文件不是目录’返回 `not a directory: ...`(line 191)、对无权限返回 `read_dir ...: <err>`(line 200)，均不命中正则，于是 line 250 直接显示英文 raw。submit() 里的同款翻译(line 400)只认 `directory does not exist`，连 canon() 的 `No such file or directory` 都不一定覆盖到所有 case。
- **影响**：小白看到突然蹦出的英文系统错误，看不懂；中英混杂破坏一致性。属边角但首次新建就可能撞上(把路径填成某个文件)。
- **建议修法**：扩展正则覆盖 `not a directory`/`permission denied`/`No such file` 等，或更稳妥地：前端不正则猜服务端文案，而是依据 ApiError.status + 一组结构化错误码(后端在 error 里加 code 字段)来选中文文案。

#### P2-126 黑板没有任何删除入口：建错/废弃的文件永久滞留，列表只增不减
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：功能完整性 / 真实用户路径　**置信度**：high
- **现象**：用户一旦建了文件（哪怕是手滑建的 typo 文件名，或一次性的临时笔记），就再也删不掉。左侧『暂无文件』很快变成一长串再也清不掉的列表。
- **复现**：—
- **根因**：crates/swarmx-server/src/routes/api.rs:73 (路由 `/api/blackboard/*path` 仅 .get().put()，无 .delete()) + web/src/components/BlackboardPanel.tsx:134 (createNew 有创建、无删除按钮)；唯一删行路径 store.rs:1947 delete_blackboard_prefix 仅由 workspaces.rs:1591 按 direction/thread 前缀调用，用户够不到单文件
- **影响**：配合上一条『撞名清空』，用户清理误建文件的唯一办法是把它 save 成空白后继续无视——既脏又容易二次误伤。小白第一次用很容易留下一堆删不掉的垃圾文件，破坏对工具的信任。
- **建议修法**：加单文件删除：后端补 DELETE /api/blackboard/*path（走 path_safe 校验 + 记一条 op=delete），前端在 pathRow 或编辑区头部加删除按钮并用 ConfirmActionDialog 二次确认。

#### P2-127 info 提示条永不自动消失，且会跨文件、跨场景残留，造成误导
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：状态诚实性 / 一致性　**置信度**：medium
- **现象**：保存后『已保存 X』、approve 后『已写入 design.approved…』这类绿色提示一直挂着不消失；切到另一个文件时 loadPath 会清掉它，但若不切文件（继续编辑同一文件、再次进入 reject 流程等）旧提示会和新状态并存。最典型：磁盘 live 变更时把 info 设成『⚠ 未保存修改』警告，之后即使你保存完成，那条 ⚠ 也可能被后来的绿色 info 覆盖或残留，时序上容易自相矛盾。
- **复现**：—
- **根因**：web/src/components/BlackboardPanel.tsx:73 (info 唯一清除点；真正缺陷是 line 403 的 textarea onChange 与 isDirty 变化时未清旧成功提示，且无 timeout 过期；line 73 原描述误记为 :74，且并无 versionPreview 清 info 路径)
- **影响**：提示条与当前真实状态脱节：用户看到『已保存』但其实又改了内容（此时 isDirty 已 true、提示却还说已保存），属于轻度状态撒谎；live 变更警告和保存成功提示打架时更容易让人误判磁盘状态。
- **建议修法**：成功类 info 用一个 2-3s 自动消失，或在 content onChange / isDirty 变 true 时清掉上一条成功提示；live 磁盘冲突警告应区别于普通 info（独立的、持续到用户处理为止的 banner）。

#### P2-128 历史抽屉的计数 badge『历史 (N)』封顶 50，会把更多版本谎报成 50
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：状态诚实性 / 功能正确性　**置信度**：medium
- **现象**：编辑区头部『历史 (N)』里的 N 来自 history.length，而 history 是用 limit=50 拉的；一个被频繁改写的文件（design.md 在修订循环里很容易超过 50 个版本）会永远显示『历史 (50)』，让用户以为只有 50 个版本。
- **复现**：—
- **根因**：BlackboardPanel.tsx:62 refreshHistory 调 listBlackboardHistory(path, 50, false)，badge 用 history.length（:302）。后端 blackboard_history(routes/swarm.rs:243) 直接 `.take(limit)`，不返回总数，所以前端拿不到真实总量，只能显示被截断后的条数。
- **影响**：轻度信息失真；对依赖历史审计『这个 design 改了多少版』的操作者会给出错误印象，且第 51+ 个旧版本在 UI 里完全不可达。
- **建议修法**：badge 文案在到达 limit 时显示『50+』；或让后端 history 接口附带 total 计数，前端据此显示真实总数并支持分页/加载更多。

#### P2-129 多个交互按钮缺 aria-label，纯符号按钮（+ / ↻ / ✓ / ✗）对屏幕阅读器不可读
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：可访问性　**置信度**：high
- **现象**：创建(+)、刷新(↻)、通过(✓ 通过)、驳回(✗ 驳回)、历史、保存等按钮只有 title 属性或符号文本，没有 aria-label；纯符号的 + 和 ↻ 对读屏用户朗读为『加号按钮』『未标注按钮』，语义全靠 title（title 不被所有读屏稳定朗读）。
- **复现**：—
- **根因**：真正缺可访问名称的元素：web/src/components/BlackboardPanel.tsx:245(纯符号 `+`，仅 title)、:248(纯符号 `↻`，仅 title)、:401(编辑 textarea 无 aria-label/关联 label)；以及 web/src/components/SwarmPanel.tsx:164(WS 状态点仅以颜色+非交互 span 的 title 表达状态)。修正：原根因中 SwarmPanel.tsx:149-163 的 tab 按钮定位有误——它们有可见文本 {TAB_LABELS[t]} 且无 title，可访问性正常；BlackboardPanel.tsx:279/287/297/305 的 ✓通过/✗驳回/历史/保存按钮均有可见中文文本作可访问名称，不属名称缺失，应从清单剔除。
- **影响**：键盘+读屏用户无法可靠分辨这些按钮的作用；纯图标按钮是 WCAG 名称缺失的典型问题。
- **建议修法**：给所有图标/符号按钮补 aria-label（创建文件/刷新列表/批准设计/驳回设计/查看历史/保存）；给编辑 textarea 加 aria-label 或关联的可见 label；WS 状态点用 aria-label 暴露『协作连接：已连接/连接中/已断开』。

#### P2-130 切换文件的放弃确认只覆盖『点列表切换』，create/历史版本预览等路径绕过了脏检查
- **页面/域**：黑板/Swarm 面板 + 通用组件　**维度**：数据完整性 / 一致性　**置信度**：medium
- **现象**：有未保存修改时点左侧另一个文件会弹『放弃未保存的修改？』确认（好）；但点 + 新建文件时，createNew 先写盘再 openPath，确认框在文件已创建之后才弹；且 openVersion 查看历史版本、approve/reject 等都不触发脏检查。create 撞名清空（见 P0）也不会先问。
- **复现**：—
- **根因**：脏检查只在 openPath(BlackboardPanel.tsx:90-102) 里做；createNew 直接调 writeBlackboard 再 openPath，时序倒置；openVersion(:104) 把 versionPreview 设上后只读展示，不算丢数据但与确认范式不统一。
- **影响**：确认弹窗范式覆盖不全，给用户『改动受保护』的错觉，实际有几条路径能绕过，体验自相矛盾。
- **建议修法**：把脏检查抽成统一守卫，create/切换/查看历史版本前都先走它；createNew 在写盘前先判断当前 buffer 是否脏并确认。


---

## 被驳回的误报（40）

复核员证明这些“疑似问题”其实不成立，列出仅供参考：

1. [首页/工作空间列表 + 新建空间向导] 向导扫描完成靠‘看到任意 project.summary.* 写入’判定，并发/残留 scout 会让向导提前误关并跳错房间
2. [首页/工作空间列表 + 新建空间向导] 向导原生选目录按钮(Tauri)选中后未做存在性/目录校验即可能直接放行
3. [首页/工作空间列表 + 新建空间向导] Welcome 文档外链硬编码到 github.com/mugsun/swarmx-core，与实际仓库可能不符
4. [对话主面板 MessagesPanel(核心)] composer 在无活成员且无 onSend 时禁用，但占位文案/提示未明确告诉小白「先唤醒」
5. [工作空间外壳 + 侧边栏 + 工具条] 未读计数依赖最近 200 条消息的 id→sender 映射，超过 200 条历史时 message_read 可能无法递减、未读 badge 卡住
6. [依赖图 DAG 视图] interrupt-all 返回 failed 列表被前端丢弃，部分 agent 暂停失败仍显示成功
7. [回放视图 + 播放器 + 录制面板] legacy RecordingsPanel 全英文逻辑零 i18n + 过滤需手动回车，与 Replays 范式不一致
8. [MCP 管理页] env 探测失败时 nodeOk 默认 true → 开关仍可点、且不显示「未检测到 Node」警告（乐观状态当已确认）
9. [MCP 管理页] 两边 key 不一致（drift）时，单独启用另一个 CLI 会静默用 claude 的 key 覆盖，不警示也不让用户先统一
10. [MCP 管理页] 文档外链 href 直接取 meta?.docsUrl，SERVERS 里若出现目录中不存在的 id 会渲染成 href=undefined 的死链（导航回当前页）
11. [用量与成本页 Usage] 每日趋势图取的是最早 90 天而非最近 90 天，长期用户图表会冻结在远古数据
12. [用量与成本页 Usage] fmtRate 用 toFixed(3)，小于 0.0005 的费率在编辑框被四舍五入显示为 0（潜在精度丢失）
13. [用量与成本页 Usage] trustHint 文案前后端措辞不一致且与实际跳转目标不符（drawer vs 聊天页）
14. [任务页 Tasks] set_task_status 对不存在的 agent_id 静默成功（影响 0 行仍返回 ok:true）
15. [文件页 Files] 列表项与“..”按钮无键盘焦点样式、无 aria-label，纯 emoji/图标无可访问名
16. [定时任务页 Cron] 提示词(prompt)无长度上限/字符校验，超长或特殊内容直接入库并投递给编排器
17. [定时任务页 Cron] preview/run 等接口请求在组件卸载后仍 setState，且 preview 轮询每次输入都打后端无本地校验前置
18. [通知中心 + 通知弹窗] markRead 的 X 按钮纯乐观、对 agent→agent 消息服务端静默无效且无任何反馈
19. [通知中心 + 通知弹窗] popover live 订阅只在 open 时 append,关闭期间到达的事件不补;打开看到的「最近」可能漏掉刚才几条
20. [设置页 Settings] 插件页「复制命令」在无 navigator.clipboard 时静默失败，按钮点了像没反应
21. [设置页 Settings] 更新失败仅 console.warn + 一行红字，未暴露具体原因也无重试入口
22. [调试页 Debug] 生产环境隐藏方式正确，但缺乏构建期硬保证、无 nav 入口（信息确认，非缺陷）
23. [全局外壳/命令面板/模型选择/Spell启动/Agent抽屉] 命令面板「设置」分组伪造 ⌘1–⌘7 快捷键，这些键根本不存在
24. [黑板/Swarm 面板 + 通用组件] selected 文件被外部删除/改名时，live 刷新会把编辑区静默清空但不告知，且历史按钮文案变 0
25. [黑板/Swarm 面板 + 通用组件] 共享区为空/未选文件时只有一行灰字，未复用 EmptyState，与项目其余空态不一致且无引导
26. [黑板/Swarm 面板 + 通用组件] approve/reject 与磁盘 save 之间无防重与禁用联动，gateBusy 仅护住 gate 两键
27. [Agent 生命周期接口] interrupt 在 Ctrl-C 发送成功后无条件广播 AgentState::Idle —— 可能对 UI 撒谎「已停下/空闲」
28. [Agent 生命周期接口] interrupt-all 静默跳过『registry 里活着但 workspace_id 为 NULL』的 agent —— 用户以为全部中断，实则有漏网
29. [Agent 生命周期接口] resume 在『已清除 paused、但 manual wake 失败』时返回 500，且对『agent 同时被 kill』场景给出误导性错误
30. [Agent 生命周期接口] 活动接口 agent_activity 不校验 agent 归属、对任意/不存在 id 一律 200 返回 []，越权读取无边界（单机模型下仅为信息层面）
31. [Agent 生命周期接口] interrupt-all 顺序逐个中断、无批量原子性，长列表下产生『部分已停部分还在』的中间不一致窗口
32. [Spell / Role / Plugin 接口] GET /api/plugins 每次请求都对每个插件 fork 一个 `<binary> --version` 子进程，无缓存
33. [Spell / Role / Plugin 接口] spell/role/plugin 三个注册表均已 include_str! 内嵌 builtin —— 安装版资源缺失问题已被正确防住（核实通过，非缺陷）
34. [文件接口] list_dir 对父目录只查一次 is_sensitive，但条目级不过滤——浏览器会暴露敏感目录/文件的存在与元数据
35. [文件接口] workspace jail 存在 canonicalize 的 TOCTOU 与 root 增删竞态，且 roots 在每次请求实时取、未对 cwd 做存在性兜底
36. [Usage 接口与计价] litellm 价格表解析失败时 fallback 静默清零，全部新模型瞬间「仅 tokens」无任何运行时告警
37. [Blackboard 与消息接口] read_blackboard 对「路径是目录」返回 500 而非 404；snapshot 的 sha/at 与文件内容可能来自不同来源
38. [WebSocket：pty / swarm / terminal] 任意本地客户端可对任意 agent_id 注入键盘/发送 Kill,无 per-agent 鉴权且 id 熵仅 32 位
39. [WebSocket：pty / swarm / terminal] /ws/swarm 把所有 SwarmEvent 无差别广播给任意连接者,跨 agent/space 无隔离
40. [Prompt 优化 / Recording 接口] 应用安装目录/HOME 变更后，旧 recording 行的绝对 path 失效 —— 已诚实降级为 404，仅记录确认无隐患
