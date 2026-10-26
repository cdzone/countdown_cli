# Changelog

本文件记录对外契约与版本边界。契约快照（`tests/fixtures/mcp-*.json`、`http-error-bodies.json`）变更时须同步更新本文件并决定版本号。

## 0.8.4

- **审计 detail 链脱敏：** `audit_detail_for_error` 对 `source()` **每一层**走 `detail_piece`（可 downcast TOML/`toml_edit`），不再只剥最外层；删掉「更深层非 `'static`」误注释。包一层 Wrap 包住 `toml::de::Error` 时，detail 仍无出错行原文、仍有 line/column。不改对外 HTTP/MCP 契约。
- **快照 version：** tools 夹具 `serverInfo.version` → 0.8.4。
- **文档：** README §6.1 `detail` 措辞收窄——配置 TOML/`toml_edit` **解析失败**仍只记位置+类型/message、不附出错行（不含标题等行内值）；**类型/业务校验**失败的说明可能引用该字段的值（如 `repeat`/`enabled`，非敏感）。`docs/mvp-arch-lessons.md` 跟到 0.8.3/0.8.4；§6.3 第 6 条改指标签 `v0.8.3`（tip `bdfcbf35` / 树 `a0f0f170`）。

## 0.8.3

- **审计 detail 脱敏：** 配置 TOML/`toml_edit` 解析失败写入审计 `detail` 时，只保留位置（line/column）与错误类型/message，不再附带出错行原文（避免标题等配置值进审计）。IO 错误仍完整记录。集中在 `audit_detail_for_error`；写路径（create/update/delete）统一走它。扩展测：损坏配置后 POST/PUT/DELETE → 500，审计全文不含标题标记。
- **MCP 连不上测收紧：** `upstream_unreachable_audit_detail_keeps_connect_reason` 改为要求 source 链片段（`tcp connect` / `os error` / `actively refused`）；裸 `e.to_string()`（仅顶层 Display）不能过。注释改准：代码不自拼 URL；reqwest Display 仍可能含 `for url (…)`。
- **快照 version：** tools 夹具 `serverInfo.version` → 0.8.3（有意保留 version 字段）。
- **文档：** README §6.1 补审计字段 `detail`（记什么/不记值；不上客户端）。

## 0.8.2

- **审计 detail 有用：** MCP 上游连不上时，审计 `detail` 不再拼 API URL，改为 `Display` + `source()` 链；`detail` 单独上限 512（caller/title/id 仍 64）。HTTP `internal`（写盘失败等）在丢弃错误前把原因写入 API 审计 `AuditLine.detail`；客户端体仍固定 `internal`。
- **快照 version：** tools 快照有意保留 `serverInfo.version`，升版须同 PR 更新夹具（与 CHANGELOG / `Cargo.toml` 一并）。
- **文档：** `docs/mvp-arch-lessons.md` 示例版号与 tip **0.8.2** 对齐；清过期行号（如「注释 L413–414」）与已更名的 `annotate`→`into_model`；§6.3 指针补 crate tip 树 `e6274143` / 标签 `v0.8.1`→`fd9edbe9`。

## 0.8.1

- **CI / 依赖：** `Cargo.lock` 将 tokio 升至 **1.54.1**（≥1.53.3），消 RUSTSEC-2026-0386（`AsyncFd` unsound）。非本闸功能引入——基线锁在 advisory 日更后同红；只动 lock，未改 `Cargo.toml` 直接依赖声明。
- **CI / web：** `web/package.json` `overrides` 钉 **zod 4.6.7**（≥4.6.6），消 GHSA-mrhx-c7xc-m3w5；重生成 `package-lock.json`。非功能闸引入——advisory 日更后 npm audit high。`npm ci` + `npm audit --audit-level=high` 绿；`npm run build` 过。未跑页面真机兼容（@ag-ui 声明可 `^3.25.18 || ^4.0.0`）。
- **文档：** README HTTP 示例码名改回 `unauthorized` / `forbidden`；标明 HTTP 自有码表与 MCP `ErrorCode` 边界映射；补 0.8.0 不兼容提示与 CHANGELOG 链接；出站说明补「不含 `target_time`」。文首/§6.3 基线 scrub。
- **契约快照诚实：** 出站快照改走生产 `into_model`/真工具（桩 API）；HTTP 错误体快照改从真实路由取响应；tools 快照补上 `instructions` 与 `serverInfo`（name/version）。去掉按措辞拦 `target_time` 的字符护栏。
- **错误文案 / 审计：** 给模型的文案按码固定短句，不再含 `http(s)://` 或完整 API URL；审计 `Record` 增加截断 `detail`；成功体先 `bytes` 再 `from_slice`，`contract_mismatch` 时 detail 保留缺失字段名。
- **HTTP `internal`：** 体 message 固定为 `internal`（不再回传 OS/IO 原文）。

## 0.8.0

- **契约闸：** 增加 MCP tools 定义 / 代表出站 / HTTP 错误体的快照测试（**0.8.0 时出站与 HTTP 体快照多为手写夹具；真接线见 0.8.1**）。
- **方案 A：** HTTP API 使用自有错误码枚举，401/403 体码改回 `unauthorized` / `forbidden`；MCP 边界穷尽映射到 `ErrorCode`（各 `invalid_*` / `payload_too_large` → `invalid_argument`）。不再写「HTTP 直接吃 ErrorCode」。
- **ModelView：** MCP 出站改为单独模型视图类型；去掉 `target_time`（及已去掉的 `remaining_seconds`），只保留 `next_*` 等模型该看的字段；工具 description / instructions 同步。
- **错误体：** 工具错误不再向模型透传上游 HTTP body；审计 `Record` 增加 `code`。固定短文案 + 审计 `detail` 在 **0.8.1** 落实（本版仍有两处把 URL/原文拼进模型文案）。
- **可观测 / CI：** `--api` / `countdown_mcp` 打印实际绑定地址；测试侧 `bind(0)` 回读端口；CI 增加 fmt、clippy `-D warnings`、MSRV 1.89。

## 0.7.3

- HTTP/MCP 引入结构化 `error.code`（P1）。
- HTTP 鉴权体码曾改为 `unauthenticated` / `permission_denied`（**0.8.0 已撤回为旧名**，见上）。

## 0.7.2

- MCP fail-closed：缺 `next_*` 字段 → `contract_mismatch`。
- MCP 出站去掉 `remaining_seconds`；list 按 `next_remaining_seconds` 排序；强调用 `next_*` 回答「多久 / 下一个」。
- **语义（影响客户端）：** `expired`、`within_hours`、`get_next_countdown` 改为按**下一次**时间（`next_*`）计算；Daily/Weekday 过当天时刻后 `expired` 为 false，且可被选为「下一个」。
- API 响应追加 `next_target_time` / `next_remaining_seconds`。
- MCP `serverInfo` 改为应用名 + 版本号（不再沿用 rmcp 默认包名版号叙事）。
