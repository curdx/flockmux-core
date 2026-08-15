<h1 align="center">swarmx</h1>

<p align="center">
  把本机真实的 <code>claude</code> / <code>codex</code> / <code>opencode</code> / <code>reasonix</code> / <code>zulu</code> / <code>kimi</code> CLI，
  组成一支会协作的 AI 蜂群——在一个浏览器标签页里跟队长说话，它自己拆解、派人、汇总。
</p>

<p align="center">
  <a href="https://github.com/curdx/swarmx/releases/latest"><img src="https://img.shields.io/github/v/release/curdx/swarmx?style=for-the-badge&label=Download" alt="Download latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/Desktop-Tauri-24C8DB.svg?style=for-the-badge&logo=tauri&logoColor=white" alt="Tauri">
  <a href="README.en.md"><img src="https://img.shields.io/badge/Lang-English-blue?style=for-the-badge" alt="English"></a>
</p>

<p align="center">
  <a href="https://github.com/curdx/swarmx/releases/latest"><strong>下载安装包 → 打开 → 跟队长说话</strong></a>
  · 全程不碰命令行
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/curdx/swarmx/main/docs/assets/hero-terminals.png" alt="swarmx 仪表盘：多个真实 CLI agent 并排运行" width="100%">
</p>

## 它是什么

swarmx 是本机多引擎编码 CLI 的**编排壳**：在 PTY 下拉起你已经装好、登录好的
`claude` / `codex` / …（每个 agent 就是那个二进制本身），再接上共享收件箱、共享黑板、
和一个统一入口的队长。

跑的是 CLI 本身，不是套壳。OAuth、限流、套餐额度跟你在终端里敲 `claude` 一模一样。
swarmx 不读、也不存你的任何 token。

**前置：** 至少有一个已登录的编码 CLI（推荐 `claude`；要自动唤醒回合的话 `codex` 需 0.132+）。
没有 CLI，swarmx 几乎是空壳——它不替你装订阅，只编排你已经有的 agent。

## 快速开始（推荐）

1. 打开 [Releases](https://github.com/curdx/swarmx/releases/latest)，按系统下载：
   - **macOS：** `swarmx_*_macos-arm64.dmg` 或 `macos-x64.dmg`
   - **Windows：** `swarmx_*_windows-x64-setup.exe` 或 `.msi`
   - **Linux：** `.AppImage` / `.deb` / `.rpm`
2. 安装并打开。
3. 新建工作空间，指向一个真实项目目录，直接跟队长说话。

server / shim / mcp 三个二进制作为 sidecar 内嵌——下载装好就能用。

<p align="center">
  <img src="https://raw.githubusercontent.com/curdx/swarmx/main/docs/assets/screenshot-orchestrator-chat.png" alt="用自然语言跟队长对话" width="80%">
</p>

## 蜂群怎么协作

跟队长说需求，它自己决定是直接做，还是拆开派几个 worker（Magentic-One：不预先画流程图，
按任务临时派）。成员靠收件箱互相寻址——消息在对方下一个回合边界投递；靠黑板共享状态——
某个 key 一被写，在等它的 agent 当场被唤醒，包括已经停下的那些。

## 进阶功能

默认路径只需要蜂群。下面两项是可选的：

- **研究委员会** — 多模型并行答题、结构化对比、综合定稿。多模型面板来自
  [Comate Zulu](https://www.npmjs.com/package/@comate/zulu)（需 license）；在「设置 → 插件」安装。
- **融合竞赛** — 同一需求丢给几个模型，各在隔离 git worktree 里实现，可挂检查命令当门禁。
  本机有两个以上可用 CLI 时开箱即用；不足时才回退 zulu。

## 从源码开发

贡献者 / 改代码时用这条路径。日常使用请走上面的安装包。

前置：Rust 1.83+、Node 22+，以及至少一个登录好的 CLI。

```bash
git clone https://github.com/curdx/swarmx.git
cd swarmx
cargo build --workspace
cd web && npm install && cd ..

# 终端 1：后端（从仓库根目录起）
cargo run -p swarmx-server          # → 127.0.0.1:7777

# 终端 2：前端
cd web && npm run dev               # → http://localhost:5173
```

隔离全栈（不碰长期 dev 会话）：

```bash
bash scripts/test-stack.sh        # build + 起在 7788/5188，数据在 /tmp
bash scripts/test-stack.sh stop
```

## 原理

```
  MCP   ─►  swarm_send_message / swarm_write_blackboard / swarm_spawn_worker …
  shim  ─►  swarmx-shim execvp 真 CLI，发 OSC ready/exit（约 95 行）
  PTY   ─►  未经修改的 claude / codex / opencode / reasonix / zulu / kimi 二进制
```

## 文档

- [配置参考](docs/configuration.md)
- [交接协议](docs/handoff-protocol.md)
- [CLAUDE.md](CLAUDE.md)：仓库工作约定、打包不变量
- [CHANGELOG.md](CHANGELOG.md)

## 安全

跟 `tmux` 一样的纯 PTY 凭据模型：不读 `~/.claude/` / `~/.codex/`，不存 token，只透传
`HOME` / `PATH`。服务端只绑 `127.0.0.1:7777`，无远程访问、无鉴权。

## 贡献

CI 硬门禁：`node scripts/harness-check.mjs`、`cargo build/test --workspace --locked`、
`web` 的 `npm run build`，以及 `directions-smoke.mjs`。真实 CLI 烟测见
`scripts/golden-cli-test.sh`。

## Star History

<a href="https://www.star-history.com/#curdx/swarmx&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date" />
  </picture>
</a>

## 许可

[MIT](LICENSE)。
