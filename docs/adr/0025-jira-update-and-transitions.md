# ADR 0025：Jira issue 更新與 transition 工具（Issue #62 / #63）

## 狀態

Accepted — 凱撒指派後執行

## 背景

目前 atlapool 的 Jira 工具只有 `jira_get_issue`、`jira_create_issue`、`jira_add_comment`。Mario agent 在實際使用時需要：

1. 更新 issue 的個別欄位（summary、assignee、description 追加），但不能直接覆寫 description，必須 append-only。
2. 查詢 issue 可用的 transition（因為每個 project/workflow 的 transition id 不同，不能硬編）。
3. 執行 transition 改變 issue 狀態。

Issue #62 與 #63 分別規格化這兩個需求，建議一起實作以共享 issue_key 驗證與 Jira client 路徑。

## 決策

新增三支 MCP 工具：

- `jira_update_issue`（write）
- `jira_get_transitions`（read）
- `jira_transition_issue`（write）

全部沿用現有的 `issue_key` 驗證（`PROJECT-NUMBER`、ASCII alphanumeric + underscore project、純數字 number），由 `issue_key` 解析 `project` 做 allowlist。write 工具走 `enable_writes` write-gate 與 fail-closed audit。

### `jira_update_issue`

參數（皆為選項，但必須至少提供一個）：

- `summary`（`string`）：整欄覆寫。
- `assignee`（`string`，accountId）：整欄覆寫，upstream body 包成 `{"accountId": "..."}`。
- `description_append`（`string`）：append-only；工具內部完成 GET → merge → PUT。

不開放 `description` 直接覆寫參數，強制 append-only，避免 agent 意外覆蓋需求。

`description_append` 處理流程：

1. 對同一 issue 發出 `GET /rest/api/3/issue/{issue_key}?fields=description`。
2. 從 `fields.description` 取得現有 ADF；若為 `null` 或缺失，建立 `{"type":"doc","version":1,"content":[]}`。
3. 在 `content` 陣列尾端加入新的 ADF content node：
   - 固定以一個 `panel` 節點包裝，格式參考 Mario 現行慣例：
     - 第一個子節點為標題 `{"type":"paragraph","content":[{"type":"text","text":"📋 分析補充"}]}`
     - 第二個子節點為 `{"type":"paragraph","content":[{"type":"text","text":"<description_append>"}]}`
   - `panel` 的 `attrs.panelType` 設為 `"info"`。
4. 對 `PUT /rest/api/3/issue/{issue_key}` 送出 `{"fields":{"summary":...,"assignee":...,"description":<merged ADF>}}`。

GET 必須在 `agent.authorize` 與 write-gate/audit attempt 之後才執行，避免未授權的 agent 洩漏 description。

### `jira_get_transitions`

- `GET /rest/api/3/issue/{issue_key}/transitions`
- read 工具，不需要 `enable_writes`
- 回傳 upstream 原始 JSON，由 caller 自行挑選 transition id

### `jira_transition_issue`

- `POST /rest/api/3/issue/{issue_key}/transitions`
- body: `{"transition":{"id":"<transition_id>"}}`
- `transition_id` 驗證：非空、僅允許 ASCII 數字（與 `page_id`/`pull_request_id` 同規則）
- write 工具，需 `enable_writes = true`，走 audit

## 影響

- `mcp.rs` 的 `ToolTarget` 需要暫存 `jira_update_issue` 的的欄位，讓 handler 在授權後才做 GET-merge-PUT。
- 需要新增 Jira mock server route（GET issue with `fields=description`、PUT issue、GET/POST transitions）。
- README tool 表格需更新。

## 風險

- `description_append` 採用固定 `panel` 模板；若 Mario agent 的 prompt 慣例改變（例如改用 `heading` 或不同 `panelType`），需要再調整。
- 兩次 upstream call（GET + PUT）增加失敗機率；GET 失敗會回傳 502/400，audit 記錄 failure。
