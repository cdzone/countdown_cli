# countdown_cli

终端倒计时 / 番茄钟小工具，可选开一个只听回环地址的 HTTP API（`--api`），另带一个只读的 MCP 服务 `countdown_mcp`，把 API 里的提醒暴露给 MCP 客户端；还带一个命令行智能体 `countdown_agent`，用本机模型通过 MCP 回答倒计时问题（第 11 节），并可开一个 AG-UI 接口和本地对话页面（第 12 节）。

本文只写鉴权、审计和部署相关的用法。完整参数以 `countdown_cli --help`、`countdown_mcp --help` 为准。

> **不兼容变更（0.5.0）：** `--api` 现在必须配 `--api-tokens`，不配就以退出码 2 拒绝启动；`countdown_mcp` 必须配 `--api-token-file` 和 `--tokens`。

> **不兼容变更（0.8.0）：** HTTP 401/403 体码改回 `unauthorized` / `forbidden`（撤回 0.7.3 的 `unauthenticated` / `permission_denied`）；MCP 出站模型视图去掉 `target_time`。详见 [CHANGELOG](CHANGELOG.md)。

## 1. 架构与两套令牌

```
MCP 客户端 ──Bearer(MCP 入口令牌)──▶ countdown_mcp ──Bearer(MCP 服务令牌)──▶ countdown_cli --api
                                       └─ MCP 审计文件                          └─ API 审计文件
```

- 一共两套令牌，**不共用**：
  - **MCP 入口令牌**：MCP 客户端（Cursor、智能体等）调 `countdown_mcp` 用，摘要登记在 MCP 自己的入口令牌表里（`--tokens`）。
  - **MCP 服务令牌**：`countdown_mcp` 调 API 用，明文放在 `--api-token-file` 指向的文件里，摘要登记在 API 的令牌表里（`--api-tokens`），只给 `countdowns:read`。
- MCP 不把客户端的令牌转给 API。调用方是谁，通过 `X-Countdown-Caller` 头带给 API，API 只记进审计的 `on_behalf_of`，**不用它做授权**（任何持有 API 令牌的程序都能伪造，审计里当作「自称」）。
- `countdown_mcp` 启动时会检查：`--api-token-file` 那张令牌的摘要如果也出现在入口令牌表里，就以退出码 2 退出，提示里不打印令牌和摘要。

## 2. 生成令牌

令牌是静态随机串，**至少 32 字节熵**。推荐：

```sh
umask 077
openssl rand -hex 32 > admin.token     # 64 个十六进制字符 = 32 字节熵
openssl rand -hex 32 > mcp-svc.token
```

服务端令牌表只存令牌的 SHA-256 摘要，不存明文。算摘要时不要带换行：

```sh
tr -d '\n' < mcp-svc.token | sha256sum | cut -d' ' -f1
# 或：echo -n "$TOKEN" | sha256sum
```

程序本身不提供生成令牌的子命令。

## 3. 文件格式

### 3.1 令牌表（API 的 `--api-tokens`；MCP 的 `--tokens` 同格式）

TOML，每个调用方一段 `[[token]]`：

```toml
[[token]]
id = "admin"                        # 写进审计的名字，不是秘密
sha256 = "<64 位十六进制摘要>"
scopes = ["countdowns:write"]

[[token]]
id = "mcp-svc"
sha256 = "<64 位十六进制摘要>"
scopes = ["countdowns:read"]
```

规则（任何一条不满足都拒绝启动，退出码 2）：

- 至少一段 `[[token]]`；空文件、只有注释、`token = []` 都算空表。
- 每段必须恰好有 `id`、`sha256`、`scopes` 三个字段；多出的字段、拼错的表名（如 `[[tokens]]`）都报错。
- `id`：1–64 个字符，只允许 `A-Z a-z 0-9 . _ -`，不能重复。
- `sha256`：恰好 64 个十六进制字符，大小写都行；只差大小写的两条算重复摘要；空串的摘要（`e3b0c442…b855`，通常是 `$TOKEN` 没设）被拒绝。
- `scopes`：必须写，必须是列表，可以为空列表（能通过认证，但没有任何权限）。只认 `countdowns:read`、`countdowns:write`，其他值报错。
- 报错只给行号或 `id`，不回显文件内容。
- 只在启动时读，改了要重启（不热加载）。

权限：

| scope | 允许 |
|---|---|
| `countdowns:read` | `GET /countdowns`、`GET /countdowns/{id}` |
| `countdowns:write` | `POST /countdowns`、`PUT` / `DELETE /countdowns/{id}`，**并包含读** |

不在上表里的方法或路径（如 `PATCH /countdowns/{id}`、`GET /nope`），令牌有效也一律 403，不会落到 404/405。令牌无效时任何路径都是 401。

### 3.2 MCP 服务令牌文件（`countdown_mcp --api-token-file`）

- 一行明文令牌。
- Unix 上文件权限必须满足 `mode & 0o077 == 0`（例如 0600 或 0400），组或其他人有任何权限都拒绝启动：`chmod 600 mcp-svc.token`。
- 只去掉末尾**一个** `\n` 或 `\r\n`，不做其他 trim。去掉后为空，或含空格、控制字符等不可见字符（只允许可见 ASCII），都拒绝启动，退出码 2。
- 报错只写路径和原因，不打印文件内容。

## 4. 启动参数

### 4.1 `countdown_cli`

| 参数 | 环境变量 | 说明 |
|---|---|---|
| `--api <地址>` | — | 开 HTTP API，只接受回环地址（`127.0.0.1:PORT` 或 `[::1]:PORT`），否则退出码 2 |
| `--api-tokens <路径>` | `COUNTDOWN_API_TOKENS` | API 令牌表，带 `--api` 时必填 |
| `--audit-log <路径>` | — | API 审计文件，默认是配置文件（`-c`）同目录下的 `countdown-audit.jsonl` |

启动顺序固定为：地址 → 令牌表 → 审计文件 → 读配置。前一步失败后面都不做，所以被拒的启动不会改配置文件，也不会生成审计文件。

```sh
countdown_cli -c config.toml --api 127.0.0.1:8080 --api-tokens api-tokens.toml
```

### 4.2 `countdown_mcp`

| 参数 | 环境变量 | 说明 |
|---|---|---|
| `--api-url <URL>` | `COUNTDOWN_API_URL` | API 地址，默认 `http://127.0.0.1:8080` |
| `--addr <地址>` | `COUNTDOWN_MCP_ADDR` | MCP 监听地址，默认 `127.0.0.1:8765`，只接受回环地址 |
| `--api-token-file <路径>` | `COUNTDOWN_API_TOKEN_FILE` | MCP 服务令牌文件（3.2 节），必填 |
| `--tokens <路径>` | `COUNTDOWN_MCP_TOKENS` | MCP 入口令牌表（3.1 节格式），必填。入口不看 scope，令牌有效即可调用三个只读工具；但每条仍须写合法的 `scopes`（如 `["countdowns:read"]`），否则令牌表校验不过 |
| `--audit-log <路径>` | `COUNTDOWN_MCP_AUDIT_LOG` | MCP 审计文件，默认当前工作目录下的 `countdown-mcp-audit.jsonl`。**不要和 API 用同一个文件** |

启动时 MCP 用服务令牌调一次 `GET /countdowns?enabled=true`：API 返回 401/403 就以退出码 2 退出；API 连不上照常启动（API 可能后起），审计里记一行 `outcome: upstream_unreachable`；API 返回其他非 2xx 也照常启动，记一行 `outcome: error`。探测成功不记。

启动顺序：地址 → 入口令牌表 → 服务令牌文件 → 混用自查 → 审计文件 → 探测，前一步失败就以退出码 2 退出（地址绑定失败为 1），不生成审计文件。启动失败的那一行提示写 stderr；正常启动时 stderr 只有一行 `countdown_mcp listening on http://<地址>/mcp`，之后运行期的错误都写审计，不写 stderr。审计文件写不进时是唯一的例外：往 stderr 打一行，只含这条记录的 `request_id` 和 `action`，不含令牌。

```sh
countdown_mcp --api-token-file mcp-svc.token --tokens mcp-tokens.toml
```

MCP 入口的 `Origin` 校验：请求不带 `Origin` 放行；带了但不是 `http://127.0.0.1:<MCP 端口>` 或 `http://localhost:<MCP 端口>`，即使令牌有效也返回 403。这个允许列表是固定的，没有参数可改。

### 4.3 只读工具（`tools/list`）

客户端连上后拿到的工具清单（名字 + `description` + 参数表）是模型选型的依据。本版三个工具：

| 工具 | 用途 |
|---|---|
| `get_next_countdown` | 无参。服务端挑 `next_remaining_seconds >= 0` 且最小的一条；没有则返回 `{"countdown":null}`，不拿过期的充数。问「最近／下一个／还有多久」用它，读返回的 `next_*`。 |
| `list_countdowns` | 默认仍返回全部（可用 `within_hours` / `include_disabled` 过滤）；按 `next_remaining_seconds` 排序；每条带服务端算的 `expired`。问「有哪些／哪些过期了」用它；**不要**用它来猜最近。 |
| `get_countdown` | 按 id 取一条（出站字段含 `next_*`，不含 `remaining_seconds`）。 |

服务端 `initialize` 的 `instructions` 用三句话复述上述分工。工具无单独版号，行为变更随应用版本（如 `0.8.4`）发布。

## 5. 认证与失败返回

- 只认 `Authorization: Bearer <令牌>` 请求头，不认查询串。`Bearer` 不区分大小写。有多个 `Authorization` 头时只看第一个。
- API：

| 情况 | 状态码 | `WWW-Authenticate` |
|---|---|---|
| 没带 `Authorization` | 401 | `Bearer realm="countdown", scope="countdowns:read"` |
| 只有 `Bearer` 没有令牌 | 401 | `Bearer realm="countdown", error="invalid_token", scope="countdowns:read"` |
| 不支持的方式（如 `Basic xxx`） | 401 | 同「没带」，不带 `error=` |
| 令牌不认识 | 401 | 带 `error="invalid_token"` |
| scope 不够 | 403 | `Bearer error="insufficient_scope", scope="countdowns:write"` |
| 方法或路径未登记 | 403 | `Bearer error="insufficient_scope"` |
| 写请求时审计写不进 | 500 | body `{"error":{"code":"audit_unavailable","message":"audit log unavailable"}}` |

  头格式坏了有意返回 401 而不是 400。scope 在解析 body 之前判，所以只读令牌发格式错的 POST 得到 403。

- MCP 入口：没带、格式不对或令牌无效一律 HTTP 401（带 `WWW-Authenticate: Bearer realm="countdown"`，不带 scope；`error="invalid_token"` 的规则同 API：只有 `Bearer` 没令牌或令牌未知才带，`Basic xxx` 不带），在进入 MCP 协议处理之前拦下，未知路径（如 `/nope`）也一样；多个 `Authorization` 头只看第一个。403 只用于坏 `Origin`（先认证，令牌无效时即使 Origin 也坏也是 401）。
- API 拒绝 MCP 的服务令牌时，工具结果 `isError: true`，正文固定为 `countdown API rejected the MCP service credential (HTTP 401)`（或 403），不附 API 返回的 body；`structuredContent.error.code` 为 `unauthenticated`（401）或 `permission_denied`（403）。
- 其他失败也是 `isError: true` 的工具结果：`content` 为人读句，**`structuredContent.error = { code, message }`** 为机读码（**MCP 侧 `ErrorCode`**；HTTP 用自有 `HttpErrorCode`，401/403 为 `unauthorized`/`forbidden`，边界映射到 MCP，见 docs §6.4）。审计 `outcome` 分别为：API 连不上 `upstream_unreachable`，API 返回 404 `not_found`，契约错配（如缺 `next_*`）`error` + 应用码 `contract_mismatch`，其他非 2xx `error`；上游拒绝凭据为 `upstream_auth_failed`，并带 `upstream_status`。客户端/夹具**只认 `code`，禁止靠 message 子串分支**。

示例（令牌已打码，完整输出见 PR 附件）：

```sh
$ curl -s -i http://127.0.0.1:8080/countdowns
HTTP/1.1 401 Unauthorized
www-authenticate: Bearer realm="countdown", scope="countdowns:read"
{"error":{"code":"unauthorized","message":"missing bearer token"}}

$ curl -s -i -H "Authorization: Bearer $(cat mcp-svc.token)" -X POST \
    -H 'Content-Type: application/json' -d '{"title":"x"}' http://127.0.0.1:8080/countdowns
HTTP/1.1 403 Forbidden
www-authenticate: Bearer error="insufficient_scope", scope="countdowns:write"
{"error":{"code":"forbidden","message":"insufficient scope"}}
```

## 6. 审计日志

JSON Lines，一行一条记录，权限 0600。API 和 MCP 各写各的文件，不写 stdout/stderr（API 会打花终端界面）。

### 6.1 字段

`ts`（本地时间，RFC 3339 带偏移）、`svc`（`api` / `mcp`）、`phase`、`request_id`、`caller`（认出的令牌 `id`，认证失败为 `null`）、`on_behalf_of`（仅 API，取 `X-Countdown-Caller`，自称未验证，最长 64 字符）、`action`（API 为方法加路由模板，如 `DELETE /countdowns/{id}`；MCP 工具调用为 `tool:<名字>`，启动探测为 `probe:GET /countdowns`，入口 401 和坏 Origin 的 403 记 `http:<方法> <实际路径>`，如 `http:POST /mcp`、`http:GET /nope`；路径是客户端随意给的，只留前 64 个字符（按字符截，不拆 UTF-8））、`target`（提醒 id）、`params`（参数摘要；API 写请求只记出现了哪些字段和 `title_len`，不记值）、`outcome`、`auth_fail`（`missing` / `malformed` / `unknown_token` / `insufficient_scope` / `bad_origin`）、`status`、`is_error`（仅 MCP 工具调用）、`upstream_status`（仅 MCP，上游返回了状态码时）、`detail`（运维侧失败原因，最长 512 字符；有则只进审计文件 0600，**不上** HTTP/MCP 响应体与模型文案。内容为错误链；配置 TOML/`toml_edit` **解析失败**时只记位置（line/column）与错误类型/message，**不附出错行原文**，故不含标题等行内配置值；**类型/业务校验**失败（如 `repeat` 拼错、`enabled` 类型错）的说明文案可能引用该字段的值（非敏感）。IO/连不上等非解析错误仍记完整链。不记令牌、请求体/响应体原文）、`duration_ms`。MCP 入口拒绝的行每条新生成一个 `request_id`，`params` 为 `null`。

**绝不记**：`Authorization` 头、令牌明文、令牌摘要或其任何前缀、请求体和响应体原文、上游错误 body。

### 6.2 一个请求记几行

- **写请求（POST / PUT / DELETE）记两行，`request_id` 相同：**
  1. 执行前写 `phase: "attempt"`（没有 `outcome`、`status`、`duration_ms`）。**写不进去就返回 500 `audit_unavailable`、不执行，这条请求在审计里没有任何记录（失败即关闭）。**
  2. 执行后写 `phase: "result"`。这一行写不进去时操作已经做完，照常返回，响应头加 `X-Audit-Status: unavailable`。
  对账时，只有 attempt、没有 result 的写请求算「结局未知」。
- **读请求和被拒的请求（401 / 403 等）只记一行 `phase: "result"`。** 这一行写不进去时照常返回，响应头加 `X-Audit-Status: unavailable`。

实际输出（一次被 403 的写、一次成功的写）：

```json
{"ts":"2026-10-22T18:55:17.764+08:00","svc":"api","phase":"result","request_id":"a6ec31c5-…","caller":"mcp-svc","on_behalf_of":null,"action":"POST /countdowns","target":null,"params":null,"outcome":"forbidden","auth_fail":"insufficient_scope","status":403,"duration_ms":0}
{"ts":"2026-10-22T18:55:17.782+08:00","svc":"api","phase":"attempt","request_id":"a6e7469c-…","caller":"admin","on_behalf_of":"doc-demo","action":"POST /countdowns","target":null,"params":{"fields":["title","datetime","repeat"],"title_len":4}}
{"ts":"2026-10-22T18:55:17.782+08:00","svc":"api","phase":"result","request_id":"a6e7469c-…","caller":"admin","on_behalf_of":"doc-demo","action":"POST /countdowns","target":null,"params":{"fields":["title","datetime","repeat"],"title_len":4},"outcome":"ok","status":201,"duration_ms":31}
```

### 6.3 两边对账

每次 MCP 工具调用生成一个 UUID，作为 `X-Request-Id` 发给 API。MCP 审计那一行和 API 审计那一行的 `request_id` 相同；MCP 工具结果的 `_meta.request_id` 也是这个值，拿它可以在两边审计里各找到一行。入口 401 和坏 Origin 的 403 只在 MCP 侧有记录（initialize、tools/list 这类协议层请求本版不记，见第 10 节）；直接调 API 的请求只在 API 侧有记录。API 收到的 `X-Request-Id` 不是合法 UUID 时不用它，自己生成一个。

### 6.4 轮转（logrotate）

服务本身不轮转。每条记录都是现开文件、整行一次写出、写完就关，不长期持有文件句柄，所以外部 logrotate 用**默认的重命名方式**即可：改名后下一条记录自动在原路径新建文件（0600），不需要发信号；改名那一刻正在写的那一条落进旧文件，不会丢。

**禁止 `copytruncate`**：它在复制和截断之间有丢失窗口，会丢审计行、副本末尾可能出现半行，「写不进审计就不执行」的保证会失效。

示例（`/etc/logrotate.d/countdown`，路径按实际改）：

```
/srv/countdown/countdown-audit.jsonl /srv/countdown/countdown-mcp-audit.jsonl {
    daily
    rotate 30
    compress
    delaycompress
    missingok
    nocreate
    notifempty
    # 不要写 copytruncate
}
```

## 7. 轮换令牌

令牌表可以同时放多条，轮换不用停机等客户端：

1. 生成新令牌，把摘要加进服务端令牌表，新旧两条都留着（`id` 不同）。
2. 重启服务端，让新表生效。
3. 改调用方的令牌文件（或环境变量），重启调用方。
4. 在审计里确认旧 `id` 不再出现，删掉旧条目，再重启一次服务端。

重启 `--api` 会让终端倒计时界面重来一次。

## 8. 客户端配置

Cursor `mcp.json`（令牌从环境变量取，环境变量要放在 Cursor 进程能看到的环境里；远程服务不支持 `envFile`）：

```json
{
  "mcpServers": {
    "countdown": {
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer ${env:COUNTDOWN_MCP_TOKEN}" }
    }
  }
}
```

MCP Inspector CLI（空的 `MCP_STORAGE_DIR` 加 `--stored-auth-only`，避免触发交互式 OAuth）：

```sh
# 不带令牌：退出码 3
MCP_STORAGE_DIR=$(mktemp -d) npx @modelcontextprotocol/inspector@2.12.0 --cli \
  http://127.0.0.1:8765/mcp --transport http --stored-auth-only --method tools/list
# 带令牌：退出码 0
MCP_STORAGE_DIR=$(mktemp -d) npx @modelcontextprotocol/inspector@2.12.0 --cli \
  http://127.0.0.1:8765/mcp --header "Authorization: Bearer $COUNTDOWN_MCP_TOKEN" \
  --transport http --stored-auth-only --method tools/list
```

## 9. 与 MCP 授权规范（2026-07-28）的偏差

- 规范写明授权对 MCP 实现是 OPTIONAL，支持时 SHOULD 遵循。本项目**整体不采用**那份 OAuth 2.1 方案，改用静态 Bearer 令牌。
- 因此不提供 Protected Resource Metadata（`/.well-known/oauth-protected-resource`），`WWW-Authenticate` 里也不带 `resource_metadata`。代价：标准 MCP 客户端没法自动发现怎么登录，只能像第 8 节那样手动配请求头。
- 受众隔离靠「每层一张令牌表 + MCP 启动自查」，不是令牌里的 `aud` 字段。API 看不到 MCP 的入口令牌表，其他交叉混用（比如把 API 写令牌也加进 MCP 入口表）程序查不出来，要靠运维约定。
- 按规范照做的部分：每个 HTTP 请求都要带令牌；令牌不放查询串；无效令牌 401；`Origin` 无效 403；不透传客户端令牌；服务端只存摘要。

## 10. 已知限制

- 两个服务都只听回环地址（`127.0.0.1` / `::1`），走明文 HTTP，没有 TLS。要对外开放必须先加 TLS 和别的认证方式，不在本版范围。
- 不做令牌热加载、限流、日志外送和服务内轮转；没有终端用户级别的权限，只认到「哪个程序在调」。
- API 和 MCP 同写一个审计文件没有测过，不要这样配。
- MCP 审计目前只记工具调用、启动探测异常和入口拒绝；规格要求的协议层请求（initialize、tools/list 等）的 `rpc:<方法>` 行本版还没有实现。
- `remaining_seconds` / `target_time` 对 Daily/Weekday 是**本周期 elapsed 显示**（过点后为负，终端可显示「已过去多久」；Weekday 同）。Agent「下一个／过期／N 小时内」应看 API 追加的 `next_target_time` / `next_remaining_seconds`（`next_occurrence`）。MCP 的 `get_next_countdown`、`expired`、`within_hours` 已改吃 `next_*`，且工具回给模型的 JSON **不再包含** `remaining_seconds` 与 `target_time`；旧 API 缺 `next_*` 时 MCP 返回工具错误（fail-closed）。旧字段语义留给终端。

## 11. 命令行智能体 `countdown_agent`

```
你 ──问题──▶ countdown_agent ──▶ 本机模型网关（Ollama，OpenAI 兼容接口）
                   │                └─ 决定调哪个工具
                   └──Bearer(MCP 入口令牌)──▶ countdown_mcp ──▶ countdown_cli --api
```

智能体只经 MCP 的三个只读工具 `list_countdowns`、`get_countdown`、`get_next_countdown` 取数，不直连 API，也不知道 API 地址。问「最近／下一个」时应走 `get_next_countdown`（由服务端挑一条未到期的）；问「有哪些／哪些过期了」走 `list_countdowns`（默认仍返回全部，每条带服务端算的 `expired`）。

### 11.1 准备

1. 在 MCP 的入口令牌表（`--tokens`）里给智能体**单独登记一行**，只给 `countdowns:read`，不要和 MCP 调 API 的服务令牌共用：

   ```sh
   umask 077
   openssl rand -hex 32 > agent.token
   tr -d '\n' < agent.token | sha256sum | cut -d' ' -f1   # 填进下面的 sha256
   ```

   ```toml
   [[token]]
   id = "countdown-agent"
   sha256 = "<上面算出的摘要>"
   scopes = ["countdowns:read"]
   ```

   `agent.token` 的格式和权限要求同 3.2（一行明文、`chmod 600`）。

2. 本机 Ollama 版本 ≥ v0.32.6，保持默认只听 `127.0.0.1:11434`，**不要**把 `OLLAMA_HOST` 改成 `0.0.0.0`。先确认模型会调工具：`ollama show deepseek-r1:8b` 的 Capabilities 里要有 `tools`，没有就 `ollama pull deepseek-r1:8b` 更新后再看。

### 11.2 配置（只走环境变量）

| 变量 | 默认 | 说明 |
|---|---|---|
| `COUNTDOWN_AGENT_MODEL` | 无，必填 | 模型名，须支持工具调用。代码里不写模型名；验收默认用 `deepseek-r1:8b`，换模型只改这一项 |
| `COUNTDOWN_AGENT_MCP_TOKEN_FILE` | 无，必填 | 上面的 `agent.token` |
| `COUNTDOWN_AGENT_BASE_URL` | `http://127.0.0.1:11434/v1` | 模型网关地址，主机只能是 `127.0.0.1`、`localhost`、`::1`，只支持 `http://` |
| `COUNTDOWN_AGENT_API_KEY` | 固定占位值 `ollama` | Ollama 要求非空但不校验，一般不用设 |
| `COUNTDOWN_MCP_URL` | `http://127.0.0.1:8765/mcp` | MCP 地址，主机限制同上（入口令牌不会发到别的机器） |
| `COUNTDOWN_AGENT_MAX_TOOL_CALLS` | `5` | 单个问题最多调几次工具，超了停止并回「查不到」 |

缺必填项、地址不是本机、令牌文件不合格：退出码 2，stderr 写明是哪一项。`OPENAI_BASE_URL`、`OPENAI_API_KEY` 等 `OPENAI_*` 变量设了也不起作用：地址和 key 总是显式设置，组织、项目请求头也显式清空。

### 11.3 用法

```sh
export COUNTDOWN_AGENT_MODEL=deepseek-r1:8b
export COUNTDOWN_AGENT_MCP_TOKEN_FILE=$PWD/agent.token
countdown_agent "最近的倒计时是哪个？"   # 问一次就退出
countdown_agent                          # 不带参数：一行一个问题，Ctrl-D 退出
```

- 回答写到 stdout。每次工具调用在 stderr 打一行调用记录，例如：

  ```
  调用记录 {"tool":"list_countdowns","args":{},"ok":true,"duration_ms":12,"request_id":"6f0c…"}
  ```

  `request_id` 由 MCP 在工具结果的 `_meta.request_id` 里返回，智能体不自己生成，可以拿它去对 MCP 和 API 的审计行。调用记录不含工具返回的数据和模型的文字。
- 启动时先连 MCP 并列工具：连不上或缺 `list_countdowns` / `get_countdown` / `get_next_countdown`，stderr 输出一行 JSON 告警（`"level":"alert"`，`event` 为 `mcp_unavailable` 或 `mcp_tools_missing`），退出码 2。
- 下面几种情况由程序直接回固定文案「查不到：<原因>。」，不再把结果交给模型：工具报错（`isError`）、API 拒绝（401/403）、连不上 MCP、工具超时（30 秒）、模型要调清单外的工具或参数不是 JSON 对象、超过工具调用上限。
- 模型没调任何工具就直接回答：判为失败，回「查不到」，不显示模型的回答（Ollama 不支持 `tool_choice`，没法强制调工具）。
- 推理模型的「思考」内容（`<think>…</think>` 段，或单独的 `reasoning` / `reasoning_content` 字段）不显示、不写进调用记录，也不回传给模型。
- 模型网关出错（连不上、超时 300 秒、返回格式不对）：stderr 打错误；单次提问模式退出码 1，多轮模式继续等下一行。

### 11.4 已知限制

- 第 5 步自动测只用假网关；**CLI 默认示例模型（`deepseek-r1:8b`）的真实 Ollama 联调未作为发布门槛**。0.7.1 **网页**路径曾用 `gemma4:31b` 人工确认会调 `get_next_countdown`——那是网页验收，不是 CLI 默认模型的完整联调证据。
- 智能体只回答倒计时问题：每个问题都要求模型至少调一次工具，所以打招呼、闲聊也会得到「查不到」。
- 多轮对话只保留之前的问题和最终回答，不保留工具结果；追问时模型要重新查。
- 不流式输出，回答要等模型整段生成完；推理模型会比较慢。
- 只接 OpenAI Chat Completions 格式的 `tool_calls`；较老的 Ollama（< v0.12.11）可能漏掉 `tool_call_id`，会导致解析失败，所以要求 ≥ v0.32.6。
- `async-openai` 的 `OpenAIConfig::new()` 内部仍会读一次 `OPENAI_*` 环境变量，读到的值随即被显式设置覆盖，不会被发出（有测试断言）。

## 12. AG-UI 接口与对话页面（`countdown_agent --serve`）

```
浏览器页面 / 任何 AG-UI 客户端 ──POST /agui（SSE）──▶ countdown_agent --serve ──▶ 模型网关、countdown_mcp（同第 11 节）
```

**任何 AG-UI 客户端都能接这个接口**：`POST /agui` 收 AG-UI 的 `RunAgentInput`，按 AG-UI 协议以 SSE 返回事件。本仓库的网页只是其中一种客户端，只做展示；调不调工具、「查不到」、去掉思考内容都在智能体里完成，接口不依赖网页，页面文件缺失时接口照常可用。

### 12.1 启动

```sh
export COUNTDOWN_AGENT_MODEL=deepseek-r1:8b
export COUNTDOWN_AGENT_MCP_TOKEN_FILE=$PWD/agent.token
countdown_agent --serve                    # 默认 127.0.0.1:8780
countdown_agent --serve 127.0.0.1:9000     # 只接受 127.0.0.1，其他地址退出码 2
```

环境变量同 11.2，另加：

| 变量 | 默认 | 说明 |
|---|---|---|
| `COUNTDOWN_AGENT_WEB_DIR` | 可执行文件所在目录下的 `web/dist` | 页面文件目录，启动时解析一次，不看当前工作目录 |

启动成功在 stderr 打一行 `{"level":"info","svc":"agent","event":"listening","addr":"127.0.0.1:8780"}`。页面目录不存在时另打一行 `{"level":"warn","svc":"agent","event":"web_dir_missing","web_dir":"…","reason":"先构建前端…或设置 COUNTDOWN_AGENT_WEB_DIR"}`，服务照常启动。

### 12.2 路由

| 路由 | 说明 |
|---|---|
| `POST /agui` | AG-UI 接口，`text/event-stream` |
| `GET /` | 页面 `index.html` |
| `GET /app.js` | 页面脚本（打包后的单个文件） |

页面和接口同一个端口。页面文件缺失时 `/`、`/app.js` 返回 503，正文写明缺失的路径；`/agui` 不受影响。

### 12.3 访问控制

- 所有路由先查 `Host`：只接受 `127.0.0.1:<端口>`、`localhost:<端口>`（端口为实际监听端口），缺 `Host` 或其他值一律 403，防 DNS 重绑定。
- 再查 `Origin`：不带放行（由上面的 Host 检查兜底）；带了只接受 `http://127.0.0.1:<端口>` 和 `http://localhost:<端口>`，其余 403。
- 接口不鉴权（已接受风险：只读、只听本机、单用户）。MCP 入口令牌不会出现在页面和任何事件里。

### 12.4 事件

每次运行只发这 10 种事件：`RUN_STARTED`、`RUN_FINISHED`、`RUN_ERROR`、`TEXT_MESSAGE_START/CONTENT/END`、`TOOL_CALL_START/ARGS/END/RESULT`，字段名对齐 `@ag-ui/core` 1.0.3（对拍样例在 `tests/fixtures/agui-events.json`，由 `web/scripts/gen-fixtures.mjs` 生成，Rust 测试只读这个文件，不需要 node）。

- 顺序：`RUN_STARTED` → 每次工具调用 `TOOL_CALL_START/ARGS/END` → `TOOL_CALL_RESULT` → 一组 `TEXT_MESSAGE_START/CONTENT/END` → 终止事件。
- `TOOL_CALL_RESULT.content` 是 JSON 字符串：成功 `{"ok":true,"request_id":"…","result":"…"}`，失败 `{"ok":false,"request_id":…,"reason":"<原因类别>"}`。`request_id` 来自 MCP 工具结果的 `_meta.request_id`，可对审计。
- 回答先去掉思考内容，再整段放进**一个** `TEXT_MESSAGE_CONTENT`，不逐字流式。
- 每次运行**恰好一个**终止事件，之后不再有事件：工具失败或模型没调工具，回「查不到：<原因>。」后发 `RUN_FINISHED`；模型网关出错（连不上、超时、非 2xx、无法解析）发 `RUN_ERROR`（`code` 为 `model_error`），不发 `RUN_FINISHED`。
- 每秒一行 SSE 注释心跳 `: ping`，不是 AG-UI 事件，客户端忽略即可。客户端断开后服务端最迟约 1 秒发现，随即取消这次运行：不再请求模型网关和 MCP，正在等的请求一并取消；stderr 打一行 `{"event":"run_ended","outcome":"cancelled","running":0,…}`（正常结束为 `finished`，模型出错为 `error`）。

### 12.5 构建页面

```sh
cd web
npm ci                         # 按 package-lock.json 安装，不走 CDN
npm audit --audit-level=high
npm run build                  # 产出 web/dist/index.html 和 web/dist/app.js
```

`web/dist/` 不进仓库，由 CI 构建。把 `web/dist` 放到 `countdown_agent` 可执行文件旁边，或用 `COUNTDOWN_AGENT_WEB_DIR` 指过去。`cargo build`、`cargo test` 不需要 node。

CI 里 `npm audit` 遇到高危及以上即失败。这是已接受的约定：代码不动，master 也可能因新公布的漏洞变红；遇到时先看 `npm audit` 输出，确认是新漏洞再决定怎么处理。

### 12.6 已知限制

- CLI 默认模型的真实 Ollama 联调未当发布门槛；网页侧曾用 `gemma4:31b` 做过工具调用人工确认（与 CLI 默认模型分开记）。自动测试用假网关加真实 MCP、API。
- 回答不逐字出现，要等模型整段生成完。
- 页面不保存历史，刷新即清空。
