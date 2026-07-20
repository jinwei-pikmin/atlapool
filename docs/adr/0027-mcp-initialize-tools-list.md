# ADR 0027：MCP `initialize` 與 `tools/list`（Issue #68）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸稽查發現 atlapool 的 MCP endpoint `/mcp` 只回應 `tools/call`，缺少 MCP 標準的 `initialize`（握手）與 `tools/list`（列出可用工具）。標準 MCP client 連線慣例是先 `initialize` 再 `tools/list`，才知道有哪些工具可以呼叫。

Issue #68 要求補齊這兩個方法，並為目前全部約 18 支工具定義標準 `inputSchema`。

## 決策

### `initialize`

- 不驗證 `X-Atlapool-Key`，讓 MCP client 在建立連線階段就能完成握手。
- 回傳 JSON-RPC `result`：
  - `protocolVersion`: `"2024-11-05"`（目前 MCP spec 版本）
  - `capabilities`: `{ "tools": {} }`
  - `serverInfo`: `{ "name": "atlapool", "version": <Cargo.toml version> }`

### `notifications/initialized`

- 標準 MCP client 在 `initialize` 回應後會發送 `notifications/initialized` 通知。
- 增加 `POST /mcp/notify` 路由，與 `mcp_handler` 共用同一處理邏輯；收到 `notifications/*` 方法時回傳 `202 Accepted`。

### `tools/call` 與標準 MCP client 的相容性

- atlapool 原有的 `tools/call` 把上游 JSON 直接當 `result` 回傳，但標準 MCP client（如 `rmcp`）的 `call_tool()` 預期收到 `CallToolResult`（`content: [{type: "text", text: "..."}]` + `isError` + `structuredContent`）。
- 為符合 MCP spec 並可被通用 client 直接使用，`tools/call` 現在永遠回傳 `CallToolResult`：
  - 上游為 JSON object 時，`content[0].text` 為 JSON 字串，`structuredContent` 為原始 object。
  - 上游為純文字/無法解析的 JSON 時，`content[0].text` 為原始文字，不附加 `structuredContent`。
  - 上游回應 204 或空 body 時，`structuredContent` 為 `{}`。
- 所有既有測試與 curl 範例同步更新為 `result.content[0].text` 或 `result.structuredContent`。

### `tools/list`

- 與 `tools/call` 相同，需要有效的 `X-Atlapool-Key`。
- 查詢該 agent 的 `tools` allowlist，與目前系統已實作的工具清單取交集。
- 只回傳交集內的工具，每個工具包含：
  - `name`
  - `description`
  - `inputSchema`（JSON Schema，描述參數與必填/選填）
- `tools/list` 不回應上游，只回傳本機 metadata，因此不觸發 write-gate/audit。

### 工具 schema

為所有已實作工具定義靜態 schema，包含：

- Jira：`jira_get_issue`、`jira_create_issue`、`jira_add_comment`、`jira_update_issue`、`jira_get_transitions`、`jira_transition_issue`
- Confluence：`confluence_get_page`、`confluence_create_page`、`confluence_update_page`
- Bitbucket read：`bitbucket_get_repo`、`bitbucket_get_pull_request`、`bitbucket_list_branches`、`bitbucket_list_directory`、`bitbucket_get_file_content`
- Bitbucket write：`bitbucket_create_repo`、`bitbucket_create_branch`、`bitbucket_create_commit`、`bitbucket_create_pull_request`

## 影響

- `mcp.rs` 的 `mcp_handler` 需要重構：先解析 JSON-RPC request，再根據 `method` 分派。
- 新增 `initialize_handler`、`tools_list_handler`、工具 schema 產生邏輯。
- 增加 `POST /mcp/notify` 路由，統一處理 MCP 通知類方法。
- `tools/call` 永遠回傳標準 MCP `CallToolResult`。
- README 與 `config.example.toml` 需說明 `/mcp` 支援 `initialize`/`tools/list`/`tools/call`。
- 需要新增測試：initialize 無 key、tools/list 認證與過濾、完整的 `initialize` → `tools/list` → `tools/call` 流程，並使用官方 `rmcp` client library 驗證。

## 風險

- `initialize` 不驗證 key，但這符合 MCP 慣例；`tools/list`/`tools/call` 仍保持原有認證。
- `tools/list` 只按 `tools` allowlist 過濾，不檢查 `projects`/`spaces`/`bitbucket_*` 等維度，因此可能列出 caller 實際無法呼叫維度的工具；這與 Issue 規格一致，但在維度 allowlist 過嚴時會讓工具列表「過寬」。
- 工具 schema 必須與 `resolve_target` 的參數檢查保持一致，未來新增工具時要同步更新。
- `tools/call` 回傳格式的改變是 breaking change：直接解析 `result` 的舊客戶端需要改為 `result.content[0].text` 或 `result.structuredContent`。文件與測試已同步更新。
