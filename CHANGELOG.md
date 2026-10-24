# Changelog

本文件记录对外契约与版本边界。契约快照（`tests/fixtures/mcp-*.json`、`http-error-bodies.json`）变更时须同步更新本文件并决定版本号。

## 0.8.1

- **契约快照诚实：** 出站快照改走生产 `into_model`/真工具（桩 API）；HTTP 错误体快照改从真实路由取响应；tools 快照补上 `instructions` 与 `serverInfo`（name/version）。去掉按措辞拦 `target_time` 的字符护栏。
- **错误文案 / 审计：** 给模型的文案按码固定短句，不再含 `http(s)://` 或完整 API URL；审计 `Record` 增加截断 `detail`；成功体先 `bytes` 再 `from_slice`，`contract_mismatch` 时 detail 保留缺失字段名。
- **HTTP `internal`：** 体 message 固定为 `internal`（不再回传 OS/IO 原文）。

## 0.8.0

- **契约闸：** 增加 MCP tools 定义 / 代表出站 / HTTP 错误体的快照测试。
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
