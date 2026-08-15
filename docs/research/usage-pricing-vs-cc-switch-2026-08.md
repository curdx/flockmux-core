# Usage / 价目：swarmx vs cc-switch / ccusage / LiteLLM

日期：2026-08-14  
目的：回答「计费是不是要更新」——先分清两套「计费」，再决定动什么。  
**本笔记不自动改产品代码。**

---

## 1. 先别混两套「计费」

| 名字 | 代码 | 用户感知 |
|---|---|---|
| **BillingSurface 红线** | `crates/swarmx-server/src/billing.rs` | 禁止静默把 Claude 推到 SDK/API；`SWARMX_ALLOW_PAID_TRANSPORT` |
| **用量估价** | `routes/usage.rs` + `litellm_pricing.json` + `/usage` | 从 session JSONL 刮 token → 套价目估美元 |

`docs/architecture.md` 里写「用量计费 — billing.rs」是**错指**；真正估价在 `usage.rs`。

---

## 2. 谱系：不是抄 cc-switch

[KNOWN] 本仓 commit `9a5752c`：`feat(usage): borrow ccusage's LiteLLM price table`。  
脚本注释写明：同源 BerriAI/litellm（ccusage 用的那张表）。

[KNOWN] cc-switch（farion1231）**后来也** embed LiteLLM + 可编辑价目 +（PR #4470）进程内 GitHub 运行时刷新 + above-200k tier。  
两边是**平行学 LiteLLM**，不是 swarmx 从 cc-switch 拷贝。

用户记忆「抄 cc-switch」→ [INFERRED] 把「可编辑价目 + LiteLLM 兜底」的产品形态记混了。对照仍有价值，但别当血统。

---

## 3. 别人怎么弄（对照）

### cc-switch（文档 4.4 + PR #4470）

- **采集**：proxy 请求日志 **或** Claude/Codex/Gemini session 导入（v3.13+）
- **价目**：预设按**具体 model id**（opus-4-8=$5/$25，opus-4/4-1=$15/$75…），不是一个 `opus` 糊所有
- **匹配**：强 normalization（去 `/` 前缀、`:` 后缀、日期后缀、wrapper…）
- **LiteLLM**：embed + **运行时最多刷一次** GitHub
- **Tier**：above-200k 字段
- **信任**：文档承认估算 ≠ 账单

### hermes（本仓已有研究）

- `CostResult.status`：`actual | estimated | included | unknown`
- 订阅路径可标 `included`，不装成现金账单
- 官方文档快照 + 可选实时 API

### swarmx 今天

- **采集**：transcript tailer — **仅 Claude / Codex / Kimi**；opencode / reasonix / zulu **无采集**
- **价目三层**：用户 `~/.swarmx/pricing.json` → 手写 5 条 substring primary → LiteLLM fallback
- **刷新**：`scripts/update-litellm-pricing.mjs` 手动；脚本写了 `--check`「给 CI」但 **workflows 未接线**
- **快照**：文件约 **2026-07-02**；2026-08-15 跑 `--check` → **stale** [COMPUTED]
- **无** above-200k；**无** `included` 状态；订阅用量仍按 API list 估美元

---

## 4. 「要不要更新」——结论分层

### A. 只刷 `litellm_pricing.json`？

**不够。** 旧 primary 裸 `opus` → $15/$75 会盖住现行旗舰（如 opus-4-8 的 $5/$25）。

**已落地（2026-08）：**

1. `best_rule_match` 最长 needle 获胜  
2. primary 拆成现行 Opus（4-8/4-7/4-6 @ $5/$25）与 legacy（4-1 / claude-3-opus @ $15/$75）；去掉裸 `opus`  
3. haiku → $1/$5；gpt-5.2 单独规则  
4. 刷新 LiteLLM 快照（~2536 models）  
5. Usage 文案标明估算 ≠ 订阅账单  

### B. 要不要镜像 cc-switch 全家桶？

**不要。** 只借具体 model 预设纪律与 normalization 思路。

### C. 发版节奏

| 动作 | 状态 |
|---|---|
| 修 primary matching | done |
| 刷 litellm 快照 | done（发版前再跑 `update-litellm-pricing.mjs`） |
| CI `--check` 接线 | 仍待 |
| 未采集引擎明示 | 仍待（TC-N06） |
| runtime 静默拉 GitHub | **done**（进程一次；磁盘缓存；`SWARMX_DISABLE_LITELLM_REFRESH` 可关） |
| CI `--check` 接线 | 仍待（embed 发版新鲜度） |
| 未采集引擎明示 | 仍待（TC-N06） |

---

## 5. 验收挂钩

见 `docs/test-cases/SUITE.md` **N. 用量/价目**。  
尤其：

- TC-N09：现行 Opus（如 `claude-opus-4-8`）不得套 legacy $15/$75
- TC-N05：文案含估算 / 非订阅账单
- TC-N06：引擎采集矩阵
- TC-N08：发版检查 snapshot 新鲜度

---

## 6. 来源

- 本仓：`usage.rs`、`update-litellm-pricing.mjs`、`litellm_pricing.json`、commit `9a5752c`
- cc-switch：`docs/user-manual/en/4-proxy/4.4-usage.md`；PR #4470（LiteLLM + tier + runtime refresh）
- 本仓 hermes 研究：`docs/research/hermes-borrow/hermes-agent.md`（CostResult status）
