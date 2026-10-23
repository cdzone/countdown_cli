# countdown_cli MVP：架构与可复用经验（备忘）

> 分享用定稿（2026-10-23，随 0.7.2 小闸修订）。公开仓库 `cdzone/countdown_cli`。
> 只含本项目技术内容；可供他人审查对照。

> **性质**：给后续真实传统业务对照的备忘，不是又一套需求文档。  
> **合并文件**：本路径唯一正文；禁止另起平行页。  
> **基线演进**：`v0.7.1`/`d67d0f3`（树 `f0e59b7a`）→ P0 `#13`/`2250cd3`（树 `9c2a66e5`）→ **`v0.7.2`** / `f45f9a66`（树 `7d01e35c`；MCP `next_*` 错配 fail-closed）。

## 1. 结论先行

这是一次「传统业务系统 + 只读 MCP + 本机模型 Agent + 可选网页」的最小闭环。业务真相在 `countdown_cli --api`；模型只经 MCP 调三个只读工具；「最近/下一个」这类易错意图用**专用工具 + 服务端挑条目**钉死，而不是让模型从全量列表里猜。

对后续真实传统业务：先拆出稳定 HTTP 契约与意图级工具，再挂 Agent；能用代码定的规则不要只写在提示词里。0.7.1 **网页**验收已用 `gemma4:31b` 确认调 `get_next_countdown`、答「未到期样例条目」（与 CLI 默认示例模型 `deepseek-r1:8b` 不是同一条验收路径；勿把网页截图当成 CLI/Ollama 默认模型的联调证据）。

## 2. 当前架构

### 2.1 组件与入口（A）

单 crate `countdown_cli`（`Cargo.toml`：`version = "0.7.2"`，`rust-version = "1.89"`，`default-run = "countdown_cli"`）。Cargo 自动发现 `src/bin/`：

| 进程 | 路径 | 默认监听 | 职责 |
|---|---|---|---|
| `countdown_cli` | `src/main.rs` + `src/api.rs` + `src/lib.rs` | `--api 127.0.0.1:8080` | 终端倒计时/番茄钟；可选 HTTP CRUD API；配置文件为真相源 |
| `countdown_mcp` | `src/bin/countdown_mcp.rs` | `127.0.0.1:8765`，路径 `/mcp` | 只读 MCP；不碰配置文件；只调 API |
| `countdown_agent` | `src/bin/countdown_agent.rs` + `src/bin/agent/agui.rs` | `--serve` → `127.0.0.1:8780` | 本机 OpenAI 兼容网关（默认 Ollama `:11434/v1`）+ MCP 客户端；可选 AG-UI `POST /agui` + `web/dist` 页面 |

鉴权共享：`src/auth.rs`（令牌表 SHA-256）、`src/audit.rs`（JSON Lines）。网页：`web/`（`@ag-ui/client` 1.0.3；`package.json` 的 `"version": "0.7.0"` 与 crate 0.7.2 尚未对齐——见缺口）。

### 2.2 边界（A）

```
浏览器/CLI 用户
    │  问题
    ▼
countdown_agent ──▶ 模型网关（只选型、写中文答）
    │  Bearer(MCP 入口令牌)
    ▼
countdown_mcp ──▶ countdown_cli --api
    │  Bearer(MCP 服务令牌，countdowns:read)
    │  + X-Request-Id / X-Countdown-Caller（自称，不做授权）
    ▼
CountdownStore（config.toml）
```

- **API 不暴露给 Agent**：`countdown_agent.rs` 文件头写明只经 MCP 的 `list_countdowns` / `get_countdown` / `get_next_countdown`，从不直连 API。
- **MCP 不碰配置文件**：`countdown_mcp.rs` 文件头；只读 HTTP。
- **两套令牌不共用**：README §1；MCP 启动时若服务令牌摘要出现在入口表则退出码 2。
- **三端均强制回环**：非 `127.0.0.1`/`::1` 拒绝绑定（CLI `--api`、MCP `--addr`、Agent `--serve`）。

### 2.3 数据流：三个只读工具（A）

实现均在 `src/bin/countdown_mcp.rs`：

| 工具 | 参数 | 行为 |
|---|---|---|
| `list_countdowns` | `within_hours?`、`include_disabled`（默认 false） | `GET /countdowns`；默认仍返回全部；`annotate` 加 `expired`（`next_remaining_seconds < 0`；`None` → 不标过期） |
| `get_countdown` | `id` | `GET /countdowns/{id}` + `expired` |
| `get_next_countdown` | **无参** | 拉 enabled 列表 → `pick_next`：只保留 `next_remaining_seconds >= 0`，取最小；空则 `{"countdown":null}` |

API 路由在 `src/api.rs`：`GET|POST /countdowns`，`GET|PUT|DELETE /countdowns/{id}`。`expired` **不由 API 下发**，MCP 层计算（注释 L413–414）。

Agent 侧白名单：`TOOLS: [&str; 3]`；`connect_mcp` 过滤后只把这三项交给模型；调用时非白名单名不转发（`TOOLS.contains`）。系统提示强制「问最近 → `get_next_countdown`」。

### 2.4 三层说明（选型引导）（A）

| 层 | 位置 | 作用 |
|---|---|---|
| MCP `instructions` | `MCP_INSTRUCTIONS` + `ServerHandler::get_info` → `with_instructions` | initialize 时给客户端/模型的总分工 |
| 工具 `description` | 各 `#[tool(description = "...")]` | **互指**：list 写明勿用于猜最近；get_next 写明勿用 list 代替 |
| Agent `SYSTEM_PROMPT` | `countdown_agent.rs` | 中文复述同一分工；白名单再兜一层 |

规范侧：MCP Tool 字段含 `name` / `description` / `inputSchema` 等，**无 per-tool `version`**（B：https://modelcontextprotocol.io/specification/2025-11-25/server/tools）。行为变更随应用版本（如 0.7.1）发布——与 README §4.3 一致（A）。

## 3. 关键设计取舍

### 3.1 为何插 MCP 层，而不是 Agent 直连 API（A + C）

- **已实现**：双令牌、入口 Origin 校验、审计 `tool:<名>` 与上游 `X-Request-Id` 对账、工具失败语义（`isError`）、Inspector/Cursor 等标准 MCP 客户端可接（README §8–9）。
- **取舍**：多一个进程与令牌运维成本；换来「业务 API 与模型面」解耦——API 可继续给人/脚本用（写 scope），模型面保持只读意图工具。后续换模型网关或加 Cursor 客户端时，不必改 CRUD。（「换客户端不必改 CRUD」为 C。）

### 3.2 为何单独 `get_next_countdown`，而不是改 list 默认过滤（A）

0.7.1 定稿（项目 backlog 与预审 claims-vs-impl，均不在本 git 树）：

1. **契约只加不改**：`list_countdowns` 默认仍全量；只**追加** `expired` 字段与新工具。旧客户端/旧提示不破。
2. **规则放代码**：`pick_next` / `is_upcoming` 在服务端；模型只负责选工具。网页验收前靠 `list` + 模型自挑会选到过期的「过期样例条目」。
3. **空结果诚实**：全过期返回 `countdown: null`，禁止拿过期充数。

Anthropic / AWS 工具写法强调：description 写清「做什么 / 何时用 / 何时不用」；给 Agent 的工具应按工作流意图设计，而不是裸包一层 list API（B：Anthropic *Define tools* / *Writing effective tools for agents*；AWS Bedrock Agents OpenAPI `description` 与 AgentCore 工具文档实践）。本仓库用「新开意图工具 + description 互指」落地，而不是把筛选逻辑只写进提示词。

## 4. 可迁移经验清单

| # | 经验 | 档 | 证据 |
|---|---|---|---|
| 1 | **工具按意图拆**：列表浏览 vs 「下一个/最近」分开；后者无参、服务端决出唯一条 | A | `get_next_countdown` / `pick_next`；网页验收截图（验收记录，不在本 git 树） |
| 2 | **契约只加不改**：新工具 + 可选新字段；不改旧工具默认语义 | A | claims-vs-impl #2/#8；list 默认全量 + `expired` |
| 3 | **规则放代码不放提示**：过期判定、最近挑选在 MCP；提示只做路由暗示 | A | `annotate` / `pick_next`；对比验收前后选型 |
| 4 | **三层说明一致**：instructions ↔ description 互指 ↔ Agent 系统提示；缺一层易漂移 | A | `MCP_INSTRUCTIONS`、两处 `#[tool(description)]`、`SYSTEM_PROMPT` |
| 5 | **Agent 白名单 + fail-closed**：只 offer 已知工具；工具失败或未调工具 → 固定「查不到」 | A | `TOOLS`、`missing_tools`、文件头注释 |
| 6 | **验收盯工具调用，不只盯自然语言**：标准含工具名、参数、答案业务值 | A | 验收口径；截图工具卡片 `get_next_countdown` `{}` |
| 7 | **MCP 无 per-tool version**：行为变更跟应用 semver / 标签走 | B（规范）+ A（README） | MCP Tools 规范字段列表无 `version`；README「工具无单独版号」 |
| 8 | **description 写清用/别用**（厂商共识） | B | Anthropic tool definition best practices；AWS Bedrock action `description` / AgentCore 工具清晰度要求 |
| 9 | **双令牌 + 回环默认**：模型面与业务面凭据隔离；demo 不做 TLS | A | README §1、§10 |
| 10 | **Rust 选型要点（本 MVP）** | A | `rmcp` 3.6 streamable HTTP + axum nest；`async-openai` 显式 base/key；`rust-version 1.89`（rmcp 要求 ≥1.88，见 `countdown-mcp-facts-2026-10-22.md`） |
| 11 | **同一字段两语义会透到 Agent**：`remaining_seconds` 是本周期 elapsed（Daily/Weekday 过点为负）；「下一个」须用 `next_*`（`next_occurrence`） | A | `calculate_target_time` vs `next_occurrence`；API `next_target_time`/`next_remaining_seconds`；MCP `pick_next` |

迁移到真实传统业务时的最小模板（C，流程建议）：

1. 冻结/扩展既有 HTTP API（人机共用）→ 2. 只读 MCP 包意图工具（不直接 wrap 每个 REST）→ 3. Agent 白名单 + 系统提示与 description 对齐 → 4. 用真实验收模型跑「会错选」的问句，截工具调用 → 5. 再考虑写操作与外部 IDE 接 MCP。

<!-- 粘贴说明：建议插入 countdown-mvp-arch-lessons-2026-10-23.md 的「## 4. 可迁移经验清单」与「## 5. 已知限制与后续」之间；粘贴后可将后续章节序号顺延，或保持原序号不动。独立全文见 countdown-mvp-seams-chapter-v1.md。 -->

## 闸门与缝

> 基线 `v0.7.1` / `d67d0f3` / 树 `f0e59b7a`。只写架构与可复用规矩。

### 1. 原则：业务口径写进代码，不寄托在提示词上

**结论：** 产品规则（谁算「过期」、谁算「最近」、空结果怎么说）必须由服务端函数钉死；提示词与工具 `description` 只负责**选型引导**，不负责最终裁决。模型靠的是工具 **name + description**（再加 Agent 白名单），靠不住「记得带某个参数」。

本仓库落地：

- 挑选与过期：`src/bin/countdown_mcp.rs` 的 `annotate` / `is_upcoming` / `pick_next`；API 不下发 `expired`。
- 选型引导三层对齐：`MCP_INSTRUCTIONS`、各 `#[tool(description = "...")]` 互指「用/别用」、`countdown_agent` 的 `SYSTEM_PROMPT` + `TOOLS` 白名单。
- 失败兜底：工具失败或模型未调工具 → 固定「查不到」（`countdown_agent.rs` 文件头与实现），不把模型临场话术当真相。

后续真业务照抄时：先问「这条规则错了会不会伤数据/伤人」——会，就写进代码；不会，才允许只写在说明里。

---

### 2. 工具按意图拆；契约只加不改

**结论：** 「浏览列表」与「给出下一个」是两种意图，不要共用一个会改默认语义的 list。MCP 工具字段**无 per-tool version**（规范侧）；行为变更跟应用 **SemVer / 标签**走（本仓库 `v0.7.1`）。

| 意图 | 工具 | 服务端行为 |
|---|---|---|
| 有哪些 / 哪些过期了 / 浏览 | `list_countdowns` | 默认仍返回全部；每条加 `expired`（`next_remaining_seconds < 0`；`None` 不标过期） |
| 最近 / 下一个 / 马上到期 | `get_next_countdown` | **无参**；`pick_next` 只保留 `next_remaining_seconds >= 0`，取最小；全无则 `{"countdown":null}` |
| 已知 id | `get_countdown` | 单条 + `expired` |

0.7.1 定稿刻意：**不**改 `list_countdowns` 的默认过滤；**不**靠改 description 让模型「从 list 里自己挑」。旧客户端与旧提示不被悄悄改语义；新意图用新工具承接。

---

### 3. 产品口径缝：fail-closed 与诚实空

两缝必须分清，混用会误导人或误导模型。

| 缝 | 含义 | 本仓库落点 |
|---|---|---|
| **fail-closed** | 上游失败、鉴权不过、工具链断、**契约字段缺失** → 对外表现为「查不到」/工具错误，不编造、不吞错误当成功 | Agent：工具 `isError` / 未调工具 → 「查不到」；MCP 对 404/401/403 用固定错误文案（401/403 不传 API body）；旧 API 缺 `next_*` → 反序列化失败 → `isError`（禁止成功+假空）；其他非 2xx 仍可能把上游 body 拼进工具错误消息（见 `fetch`） |
| **诚实空** | 查询成功，但业务上确实没有合格条目 → **成功 + 明确空**（`countdown: null`），禁止拿过期条目充数 | `get_next_countdown` 全过期 / 无未到期时返回 `null` |

另：**不要默认过滤过期。** 若 list 默认只留未到期，会吞掉「一次性已过期」以及「仍应出现在浏览里」的条目。本版选择：list 全量 + `expired`（按 `next_remaining_seconds`）；「下一个」走专用工具。Daily/Weekday 过点后 `next_*` 仍为正 → list 里 `expired: false`，也会被 `get_next` 选中。

---

### 4. `get_next` 判定与「同一字段两语义」

- **闸门：** `is_upcoming` ≡ `next_remaining_seconds` 为 `Some(s)` 且 `s >= 0`；`pick_next` 在此集合上取最小 `next_remaining_seconds`。
- **根因（不是「Daily 算错/未滚日」）：** `remaining_seconds` / `target_time` 来自 `calculate_target_time`，对 Daily/Weekday 是**本周期 elapsed 显示**（过点故意为负，终端「was N ago」；Weekday 周末锚上周五）。Agent「下一个」若误用该字段，会把过点重复项当过期丢掉。修法：**只加不改**——`next_occurrence` + API `next_target_time` / `next_remaining_seconds`；MCP 的 pick/expired/`within_hours` 改吃 `next_*`，且**回给模型的 JSON 不再吐 `remaining_seconds`**（避免双字段歧义）。旧字段语义留给终端（`main.rs` 读 domain，不读 API）。

---

### 5. 本轮踩过的跨缝坑（各一句）

1. **`--api` 非回环先写 `.bak`：** 启用 API 时 `CountdownStore` 会补 id 并写 `*.toml.bak`（`src/lib.rs`）。必须先校验绑定地址再建 store；被拒启动不得碰用户文件——`tests/api_addr_rejected.rs` 锁住「无 `.bak`」。
2. **MCP 默认会走 `HTTP_PROXY`：** 本机 API 若被代理劫持则工具全挂。`countdown_mcp`（及 agent 侧客户端）对上游 HTTP 显式 `.no_proxy()`；`tests/mcp_proxy_env.rs` 在死代理环境下仍能 list。
3. **CI 上 `terminal-notifier` 同步 wait 卡启动：** 异步路径里同步等子进程会堵 `--api` 起来。现策略：非 macOS 不 spawn；集成测 `hanging_terminal_notifier_does_not_block_the_api`。macOS 通知路径仍有同步 `wait`、超时未统一加——见项目 backlog 中 0.5.1 通知超时项（不在本 git 树）。
4. **跨进程新增字段：消费方勿 `serde(default)`。** 只加字段时，若 MCP 对 `next_*` 用 `#[serde(default)]`，旧 API 缺字段会静默变 `None`，`get_next` 成功返回 `null`（假空，比错选更糟）。可迁移写法：字段必现、值可 null（`deserialize_with` + `Option::deserialize`），缺了就 `isError`。
5. **网页验收必须看工具名，不只看自然语言：** 通过标准含工具名 = `get_next_countdown`（参数 `{}`）、答案为未到期业务条目、且不是列表里排前的已过期条目。只听中文答「好像对」会漏掉仍在调 `list_countdowns` 自挑的情况。

（合入窗口类坑：鉴权曾拆成多 PR 时，规格接受「中间态 master 不打标签、不外发试用」的有条件窗口。推断：真业务多 PR 拆合时同样要显式写清窗口内允许什么。）

---

### 6. 给后续真实业务的检查清单

接入 AI / MCP / Agent 前可勾选：

- [ ] **意图工具：** 列表浏览 vs 「下一个/最近/待办头」等易错问法是否拆成专用工具（无参或少参，服务端决出）？
- [ ] **服务端算状态：** 过期、权限可见范围、排序键是否在服务端计算并返回，而不是交给模型从半生数据里猜？
- [ ] **空与错分离：** 业务空 = 成功 + 明确空（显式 null OK）；上游/鉴权失败/契约缺字段 = fail-closed（`isError`），不编造、不静默 default？
- [ ] **API↔MCP 加字段：** 缺字段 → `isError` + 显式 null → 诚实空；单测与错配夹具（新消费方 + 旧生产方）齐备？
- [ ] **契约只加不改：** 新意图用新工具/新可选字段；不改旧工具默认语义；行为变更跟应用 SemVer（bump + `serverInfo`）？
- [ ] **审计可对账：** 工具调用与上游请求能用同一 `request_id`（或等价关联）对上；令牌表只存 sha256 摘要 + scopes，明文令牌放在仓库外（如 `~/countdown-local/`）？
- [ ] **令牌不进仓库：** `*.token` / 令牌表明文不进 git；入口令牌与上游服务令牌不共用？
- [ ] **合入窗口写清：** 多 PR 拆合时，中间态是否禁止打标签/外发？窗口内暴露面是否可接受？
- [ ] **验收用固定问法回归：** 预置会错选的数据（如过期条目排前）；验收盯工具名 + 参数 + 业务值，截图或日志可复现。

---

### 7. 主审收口句

本 MVP 证明的不是「模型很聪明」，而是：**把会错的选择从提示词挪进代码闸门，再让模型只做选型。** 缝（空 vs 错、list vs next、代理、启动副作用、合入窗口）写清楚，后面传统业务才能对照着加复杂度，而不是从头再踩一遍。

## 5. 「业务系统接 AI」学习要点

> 基线同备忘 `v0.7.2` / `f45f9a66`（树 `7d01e35c`）。只写可迁移的学习对照。与「闸门与缝」互补：那边钉工程缝，这边钉**怎么练、怎么验收自己学会了**。

### 5.1 先建立心智模型（三层）

接 AI 前先能用自己的话画出三层，缺一层就先补业务面，不要先调模型：

1. **数据面**：稳定、可鉴权的 HTTP（或等价）契约；谁是真相源。
2. **工具面**：只读 MCP（或同类）；每个工具对应一种**用户意图**，不是「把 API 原样暴露」。
3. **智能体面**：模型只负责选型与组织语言；业务裁决在工具实现里。

本 MVP 对照：`countdown_cli --api` → `countdown_mcp` → `countdown_agent`（+ 可选网页）。Agent **不**直连 API。

### 5.2 十条可自检要点

1. **边界先于提示词**：Agent 经只读工具取数；写操作另开工具并带确认。问自己：模型有没有机会绕过工具面？
2. **Fail-closed**：该查业务却零次工具调用 → 程序固定失败文案（本仓「查不到」），不把模型临场话术当结果。
3. **意图型工具**：高频易错问法（如「最近/下一个」）单独窄工具，无参或少参，服务端决出；全量列表留给「有哪些/哪些过期」。
4. **规则进代码 + 字段语义对齐意图**：过期、排序、「下一个」由服务端函数 + 测试钉死；`description` / 系统提示只做选型引导。「本周期 elapsed」与「下次发生」是两套语义——勿共用同一出口字段；回给模型的视图只留 unambiguous 那一套（本仓：出站不吐 `remaining_seconds`，问多久用 `next_*`）。对照：若只改说明、不改代码/出站，验收是否仍可能答错？
5. **空与错分开记**：业务空 = 成功 + 明确空（可含**显式 null**）；上游/鉴权失败/**契约缺字段** = fail-closed（工具 `isError`）。切勿用静默默认（如消费方 `serde(default)`）把「缺字段」伪装成「没有下一条」——假空比诚实空更糟。
6. **MCP 怎么被看见**：`initialize` 的 instructions + `tools/list` 的 name / description / inputSchema；工具 description 互指「用/别用」，减少抢活。
7. **模型当可换部件**：模型名不写死，走环境变量；上线前确认具备 tools；推理模型的思考段不得进回答、审计、页面。
8. **可对账才算查过**：调用记录含工具名、参数、结果类别、`request_id`（或等价）；网页验收盯**工具卡片**，不只听自然语言。
9. **断开即停**：客户端断开后取消当次运行，不留孤儿请求（本仓 SSE 心跳 + 流丢弃绑定）。
10. **密钥、合入窗口与行为升版**：入口令牌与下游服务令牌分开；明文不进仓；多 PR 中间态是否允许打标签/外发要写清。**行为变了就 bump** 应用版本 + `serverInfo`，避免同一版本号两种行为。

### 5.3 最小对照练习（换一条传统业务时）

用纸笔（或备忘一小节）只填三行，再动提示词：

| 行 | 要写出的东西 |
|---|---|
| A | 只读 API 清单（路径 + 谁算成功/失败；新字段缺了算不算失败） |
| B | 意图工具表（用户怎么问 → 调哪个工具 → 服务端怎么决；模型看到哪些字段） |
| C | 无工具调用 / 契约错配时的固定失败文案 |

填完再问：哪几条规则「错了会伤数据/伤人」？那些必须进 B 的服务端列，不能只进提示词。

### 5.4 验收自己「学会了」的信号

- 能解释为何「列表浏览」和「下一个」要拆工具，以及默认过滤过期会踩什么坑（一次性已过期仍应可浏览；Daily 过点靠 `next_*` 仍 upcoming）。
- 给定一次对话记录，能判断：调对工具了吗？是诚实空还是 fail-closed？缺字段为何必须 `isError`，显式 null 与缺字段差在哪？
- 换模型只改配置，不改业务规则代码，验收问法仍盯工具名 + 业务值。

### 5.5 与邻章分工

- 工程闸门、跨缝坑、勾选清单细节 → 见「闸门与缝」。
- crate / 测试如何钉行为 → 见「Rust / MCP 实现取舍」。
- 本章不重复展开实现细节；作学习路径与自检用。

### 5.6 出门清单（加字段 / 改 MCP 出站）

真业务或本仓再动 API↔MCP 契约前勾选（与「闸门与缝」检查单互补，专盯学习侧易漏项）：

- [ ] **API↔MCP 加字段：** 消费方字段**必现、值可 null**；缺字段 → `isError`（禁止成功假空）；显式 null → 诚实空。单测两条都有。
- [ ] **错配夹具：** 验收含一列「新消费方 + 旧生产方」（配对绿不够）。
- [ ] **模型出站视图：** 只留 unambiguous 字段；双语义字段勿并列回给模型。
- [ ] **行为变就升版：** bump 应用版本 + `serverInfo`；文档硬矛盾同闸清。

## 6. 已知限制与后续

### 6.1 已写入仓库的限制（A，README §10 / §11.4 / §12.6）

- 仅回环明文 HTTP，无 TLS、无限流、无令牌热加载。
- MCP 审计未记协议层 `initialize` / `tools/list` 的 `rpc:*` 行。
- **同一字段两语义（已修 P0）：** `remaining_seconds` 仍是本周期 elapsed（Daily/Weekday 过点为负，含 Weekday）；「下一个」改用 `next_remaining_seconds`（`next_occurrence`）。勿再写成「Daily 未滚日」。
- Agent：无工具/工具失败一律「查不到」；模型须支持 tool calls。

### 6.2 流程待办（A，`countdown-backlog.md`）

- **MCP 客户端接入调研**（Cursor / CodeBuddy / Codex：Authorization 头、每工具令牌、skill/rules）：待排期后再开；本文件不展开。
- 其他前端形态（后台服务等）；网页只是 AG-UI 的一种。
- backlog 里另有 0.5.1 通知超时、审计方法名截断等，与 0.7.1 主线无关。

### 6.3 本短文未核到的缺口

1. **未在本机重跑** 1.89/1.99.1 全测与 clippy（仅见既有日志副本，未复跑）。
2. **未打开** 远端 GitHub Actions 原始日志；CI 称绿处本文不重复断言。
3. **`web/package.json` 仍标 0.7.0**：与 crate 0.7.2 不一致是否有意，未问产品方。
4. **Anthropic「合并相关操作为更少工具」** 与「按意图拆工具」存在张力：本 MVP 选择拆开以避免模型从 list 猜；是否对所有业务成立未做对照实验（C）。
5. **外部 IDE 接 MCP** 未在本轮实测（留给后续排期后的调研）。
6. 本地 worktree 的 `HEAD` 可能落后；以已合入的 `origin/master` 为准（P0 后 tip 曾为 `2250cd3` / 树 `9c2a66e5`；本闸再叠 0.7.2）。

## 7. Rust / MCP 实现取舍

> 本章只谈实现：crate / bin 边界、MCP 工具怎么钉行为、测试钉什么。产品闸门、令牌运维、网页验收口径见其他章。基线：公开仓库 `cdzone/countdown_cli` crate **0.7.2**（MSRV 1.89，单 crate 多 bin）。

### 7.1 本章范围

结论先行：**能用代码钉死的选型与过滤，不要交给模型话术；能用测试钉住的契约，不要靠人工截图。**

写：进程/模块怎么切、依赖往哪边指、MCP 层如何落地「按意图拆工具」、测试如何钉行为、迁到真业务时实现侧对照。

不写：鉴权 fail-closed 的产品理由、审计对账的运维流程、提示词文案的业务口径（那些归闸门章）。

原则可点名：

- **边界**：谁拥有状态、谁只读代理、谁只编排模型。
- **依赖方向**：外层依赖内层契约，禁止跳层。
- **测行为不测实现**：断言可观察结果（挑谁、标不标 `expired`、提供哪些工具），不锁内部字段名或排序算法细节。

### 7.2 crate / bin 边界与依赖方向

#### 7.2.1 单 crate、多 bin

`Cargo.toml` 定一个包名 `countdown_cli`，`rust-version = "1.89"`，`default-run = "countdown_cli"`。可执行入口靠 `src/bin/` 自动发现，不拆 workspace：

| 二进制 | 角色 | 数据从哪来 |
|---|---|---|
| `countdown_cli` | 终端 UI + 可选 `--api`（业务真相源） | 本地配置 / 内存中的倒计时表 |
| `countdown_mcp` | 只读 MCP 门面 | **只**调 API（Bearer 服务令牌） |
| `countdown_agent` | 模型编排 + 可选 `--serve`（AG-UI） | **只**调 MCP 三个只读工具 |

共享库在 `src/lib.rs`（含 `auth` / `audit` 与领域类型）；`api.rs` / `notify.rs` 挂在 **`countdown_cli` 二进制**（`main.rs` 的 `mod`），不是 lib。两个旁路 bin 通过 `use countdown_cli::…` 复用鉴权读文件等，**不**各自复制一套，也**不**直接依赖 `api` 模块。

#### 7.2.2 依赖方向（谁调谁）

```
浏览器 / CLI 用户
        │
        ▼
countdown_agent  ──Bearer(入口令牌)──▶  countdown_mcp  ──Bearer(服务令牌)──▶  countdown_cli --api
        │                                      │
        └── 本机模型网关（OpenAI 兼容）          └── 不算「业务库」，只做翻译 + 标注
```

- **API 是真相源**：增删改查、剩余秒数计算都在这里。
- **MCP 是只读适配层**：把 HTTP JSON 收成工具结果；可加服务端字段（如 `expired`），不改 API CRUD。
- **Agent 不直连 API**：源码与 README 写死「never calls the countdown API」。好处：
  1. 权限面收窄到 MCP 入口令牌（只读工具），不会误拿写 scope；
  2. 工具清单与 description 是模型选型的唯一契约，跳层会让「按意图拆工具」失效；
  3. 审计对账路径统一（工具调用的 `request_id` 贯穿 MCP → API）。

#### 7.2.3 工程侧配套（与边界相关）

- **共用 `CARGO_TARGET_DIR`**：本机多工具链（如 1.89 / 更新 stable）对照编译时，指向同一目录，避免每开一个沙箱就重新拉依赖；与「单 crate」一致——产物按包名共享，不按进程拆。
- **新增依赖先报批**：`Cargo.toml` 里 MCP / Agent 相关依赖（如 `rmcp`、`async-openai`）进树前走评审；0.7.1 补丁刻意**不加**新直接依赖、**不改** `src/api.rs` CRUD，只在 MCP/Agent 层加工具与提示。

### 7.3 MCP 层：签名、annotate / pick_next、互指如何落地

#### 7.3.1 三个工具（按意图拆，不按「资源 CRUD」拆）

| 工具 | 签名要点 | 服务端保证 |
|---|---|---|
| `get_next_countdown` | **无参** | 在 `next_remaining_seconds >= 0` 中取最小；全无 upcoming → `{"countdown":null}`，**不**用过期条目充数 |
| `list_countdowns` | 可选 `within_hours` / `include_disabled`；默认仍返回全部 | 每条带服务端算的 `expired` |
| `get_countdown` | 必填 `id` | 单条 + `expired` |

意图写进 `#[tool(description = "...")]`，并互指：list 的 description 写「不要用本工具猜最近，去调 `get_next_countdown`」；next 的 description 写「不要用 `list_countdowns` 自己挑」。

#### 7.3.2 `annotate` 与 `pick_next`（行为钉在纯函数）

API **不**下发 `expired`。MCP 在出站前：

- `annotate`：`next_remaining_seconds` 为负数 → `expired: true`；为 `None` → `expired: false`（诚实：算不出下次目标就不谎称过期）。
- `is_upcoming`：`Some(s) if s >= 0`。
- `pick_next`：过滤 upcoming 后 `min_by_key(next_remaining_seconds)`。Daily/Weekday 过点后 API 给出正向 `next_*`，故仍可被选中且 list 中 `expired: false`；**一次性**已过期（`next_remaining_seconds < 0`）才在 next 中被跳过、list 标 `expired: true`。
- 出站视图：工具 JSON **不序列化** `remaining_seconds`；问「还有多久」只用 `next_*`。`next_*` 反序列化必填（缺字段 → 工具 `isError`）。

这样「最近」与「列表是否含过期」是**确定性代码**，与模型是否听话无关。

#### 7.3.3 `instructions` 与 description 互指

`initialize` 时 `with_instructions(MCP_INSTRUCTIONS)` + `with_server_info(countdown_cli / CARGO_PKG_VERSION)`：三句话复述「问最近 → next；问有哪些／过期 → list；已知 id → get」。与各工具 description 同口径，形成**双通道**：协议层 instructions + 工具级 description。工具无单独版号，行为变更随应用版本（如 `0.7.2`）发布。

#### 7.3.4 Agent 侧再钉一层白名单

`countdown_agent` 内：

```text
const TOOLS: [&str; 3] = ["list_countdowns", "get_countdown", "get_next_countdown"];
```

- 启动时 `tools/list` 后过滤，只把白名单交给模型；缺任一工具 → 告警退出。
- 运行期模型若点名白名单外工具或参数非 JSON 对象 → 程序直接回固定「查不到：…」，**不**把错误结果交给模型润色。
- `SYSTEM_PROMPT` 再次写「问最近必须调 `get_next_countdown`」——提示是辅助，**选型正确性仍以 MCP 的 `pick_next` 为准**。

失败语义统一为用户可见的「查不到：\<原因\>。」（工具 `isError`、上游 401/403、超时、超调用上限、模型零工具调用等），避免半结果或模型瞎编。

### 7.4 测试策略：钉行为，不测模型话术

#### 7.4.1 钉行为的例子（均有单测 / 集成测）

| 行为 | 测什么 | 典型用例名（源码） |
|---|---|---|
| next 混排 | 过期 + 剩余 0 + 更远 → 选剩余 0 且未过期 | `get_next_picks_nearest_upcoming_among_mixed` |
| next 全过期 | 返回显式空；list 仍全是 `expired: true` | `get_next_returns_explicit_empty_when_all_expired` |
| pick 跳过负 / None | 纯函数：负与 `None` 不入选 | `pick_next_skips_negative_and_none` |
| list 标 expired | 默认仍返回全部；负剩余标 true，`None` 标 false | `list_marks_expired_and_keeps_all_by_default` |
| Agent 提供三工具 | 假网关看到的 `tools` 长度为 3 且含上述名字 | `tests/agent.rs` 中 `offered.len() == 3` |
| 缺工具检测 | `missing_tools` 列出缺的名字 | `missing_tools_lists_what_is_absent` |
| 失败「查不到」 | `isError` / 401 / 零工具 / 超上限 → 固定中文文案，不把模型原文当答案 | agent 集成测若干条 |

纯函数测（`annotate` / `pick_next`）不启服务器；工具测用本地假上游或真实回环 API。Agent 测用**假模型网关** + 真实 MCP/API，不断言 Ollama 是否「会说话」。

#### 7.4.2 为何不测模型话术

- 模型输出非确定性，CI 无法稳定断言「必须答某某标题」。
- 产品正确性已前移到服务端挑选与 Agent 白名单；话术只是展示层。
- 真模型联调、网页截图属人工验收（规格已标明未用真实网关做自动验收），不进 `cargo test` 红线。

**测行为不测实现**：不断言内部 JSON 字段顺序、reqwest 客户端构造细节；断言可观察契约（空结果形状、`expired` 布尔、提供的工具名、失败文案前缀）。

### 7.5 工程取舍清单

| 选了什么 | 没选什么 | 理由 |
|---|---|---|
| 单 crate + `src/bin` 多入口 | cargo workspace 多包 | MVP 共享类型/鉴权成本低；发布同一版本号 |
| Agent → MCP → API | Agent 直连 API | 工具契约与权限面统一；禁止跳层破坏「按意图拆工具」 |
| MCP 侧算 `expired` / `pick_next` | 只改 description 让模型自己挑 | 0.7.0→0.7.1 的根因：只靠提示会被过期条目带偏 |
| `get_next` 无参 + 显式 `null` | 复用 list + `within_hours=…` 冒充「最近」 | 签名即意图；空与「有一条」可区分 |
| Agent `TOOLS` 白名单 + 固定「查不到」 | 把任意 `tools/list` 原样扔给模型 | 防清单膨胀与幻觉工具名 |
| 静态 Bearer、回环监听 | 本版上 OAuth / TLS 对外 | 规范允许 OPTIONAL；受众隔离靠两套令牌表（见架构章） |
| 假网关测 Agent | CI 拉真 Ollama 断言回答正文 | 稳定、快；真模型留给人工 |
| 0.7.1 不加新直接依赖、不改 API CRUD | 顺手重构 API | 契约只加不改；缩小评审面 |
| MSRV 1.89；对照测可用共用 `CARGO_TARGET_DIR` | 每环境独立 target 强隔离 | 本机多 toolchain 复用编译缓存 |

### 7.6 迁到真实业务时：实现侧对照（可执行）

1. **画出三层（或等价）并禁止跳层**：业务 API（真相）→ 工具门面（只读/按意图）→ 编排进程（模型）。代码审查时搜编排层是否直接 `reqwest` 业务库。
2. **高频意图是否有专用工具**：凡「最近 / 待办下一件 / 当前有效」类问题，是否有无参或窄参工具由服务端挑选？还是仍让模型在大列表上自己 `min`？
3. **可观察字段是否服务端标注**：过期、状态机阶段、权限裁剪结果等，是否在工具 JSON 里带布尔/枚举，而不是只写在 description 里指望模型推断？
4. **白名单与失败封闭**：编排层是否硬编码允许的工具名？清单外调用、上游 4xx、超时是否固定失败文案，而不是把原始错误或模型续写交给用户？
5. **测试是否钉契约**：至少有「混排挑选」「全无效显式空」「列表仍含无效并标注」「编排层暴露的工具集合」四类用例；CI 不依赖真实大模型回答正文。
6. **依赖与 crate 边界**：新 MCP/Agent 依赖是否经报批？业务 CRUD 与工具适配是否同版本发布、工具行为变更是否跟应用版本走（无影子工具版号即可，但要有发布说明）？

---

*材料：`v0.7.1` 树内 `README.md`、`Cargo.toml`、`src/bin/countdown_mcp.rs`、`src/bin/countdown_agent.rs`、`tests/agent.rs`；预审对照 `claims-vs-impl.md`。示例名均用虚构占位，不含真实日程标题或本机家目录路径。*

---

---

