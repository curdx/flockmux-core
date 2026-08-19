<h1 align="center">swarmx</h1>

<p align="center">
  Turn the real <code>claude</code> / <code>codex</code> / <code>opencode</code> / <code>reasonix</code> / <code>zulu</code> / <code>kimi</code> CLIs on your machine
  into a collaborating AI swarm — talk to one orchestrator in a browser tab; it decomposes, delegates, and reports back.
</p>

<p align="center">
  <a href="https://github.com/mugsun/swarmx/releases/latest"><img src="https://img.shields.io/github/v/release/curdx/swarmx?style=for-the-badge&label=Download" alt="Download latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/Desktop-Tauri-24C8DB.svg?style=for-the-badge&logo=tauri&logoColor=white" alt="Tauri">
  <a href="README.md"><img src="https://img.shields.io/badge/Lang-中文-red?style=for-the-badge" alt="中文"></a>
</p>

<p align="center">
  <a href="https://github.com/mugsun/swarmx/releases/latest"><strong>Download → install → open → talk to the orchestrator</strong></a>
  · no terminal required
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/curdx/swarmx/main/docs/assets/hero-terminals.png" alt="swarmx dashboard: multiple real CLI agents running side by side" width="100%">
</p>

## What it is

swarmx is an **orchestration shell** for multi-engine coding CLIs on your machine. It
spawns the CLIs you already installed and logged in — each agent *is* that binary —
and gives them a shared inbox, a shared blackboard, and one orchestrator you talk to.

It runs the CLIs themselves, not a wrapper. OAuth, rate limits, and plan quotas behave
exactly like typing `claude` in your own terminal. swarmx never reads or stores your tokens.

**Prerequisite:** at least one logged-in coding CLI (`claude` recommended; `codex` 0.132+
for the auto-wake loop). Without a CLI, swarmx is almost an empty shell — it does not
buy you a subscription; it coordinates agents you already have.

## Quick start (recommended)

1. Open [Releases](https://github.com/mugsun/swarmx/releases/latest) and download for your OS:
   - **macOS:** `swarmx_*_macos-arm64.dmg` or `macos-x64.dmg`
   - **Windows:** `swarmx_*_windows-x64-setup.exe` or `.msi`
   - **Linux:** `.AppImage` / `.deb` / `.rpm`
2. Install and open the app.
3. Create a workspace pointed at a real project directory and talk to the orchestrator.

The server / shim / mcp binaries ride along as sidecars — download, install, use.

<p align="center">
  <img src="https://raw.githubusercontent.com/curdx/swarmx/main/docs/assets/screenshot-orchestrator-chat.png" alt="Talking to the orchestrator in plain language" width="80%">
</p>

## How the swarm collaborates

Tell the orchestrator what you need; it decides whether to do it itself or split work
across workers (Magentic-One — no pre-declared topology). Members address each other
through the inbox (messages land at the next turn boundary) and share state on the
blackboard; write a key and every waiter wakes in the same tick, including idle agents.

## Advanced (optional)

The default path is swarm collaboration only:

- **Research committee** — parallel multi-model answers, structured comparison, synthesis.
  Panel via [Comate Zulu](https://www.npmjs.com/package/@comate/zulu) (license required);
  install from **Settings → Plugins**.
- **Fusion** — same need raced across models in isolated git worktrees, optional check
  command as a hard gate. Works out of the box with two+ local CLIs; falls back to zulu
  when fewer are available.

## Develop from source

For contributors. Everyday use should go through the installer above.

Prereqs: Rust 1.83+, Node 22+, and at least one logged-in CLI.

```bash
git clone https://github.com/mugsun/swarmx.git
cd swarmx
cargo build --workspace
cd web && npm install && cd ..

# terminal 1: backend (from repo root)
cargo run -p swarmx-server          # → 127.0.0.1:7777

# terminal 2: frontend
cd web && npm run dev               # → http://localhost:5173
```

Isolated full stack (won't touch your long-lived dev session):

```bash
bash scripts/test-stack.sh        # build + start on 7788/5188, data in /tmp
bash scripts/test-stack.sh stop
```

## How it works

```
  MCP   ─►  swarm_send_message / swarm_write_blackboard / swarm_spawn_worker …
  shim  ─►  swarmx-shim execvp's the real CLI, emits OSC ready/exit (~95 lines)
  PTY   ─►  the unmodified claude / codex / opencode / reasonix / zulu / kimi binary
```

## Docs

- [Configuration reference](docs/configuration.md)
- [Handoff protocol](docs/handoff-protocol.md)
- [CLAUDE.md](CLAUDE.md): repo conventions and packaging invariants
- [CHANGELOG.md](CHANGELOG.md)

## Security

Same PTY-only credentials model as `tmux`: doesn't read `~/.claude/` / `~/.codex/`,
doesn't store tokens, passes `HOME` / `PATH`. Server binds only `127.0.0.1:7777` —
no remote access, no auth.

## Contributing

CI hard gates: `node scripts/harness-check.mjs`, `cargo build/test --workspace --locked`,
`web`'s `npm run build`, and `directions-smoke.mjs`. Real-CLI smoke:
`scripts/golden-cli-test.sh`.

## Star History

<a href="https://www.star-history.com/#curdx/swarmx&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=curdx/swarmx&type=Date" />
  </picture>
</a>

## License

[MIT](LICENSE).
