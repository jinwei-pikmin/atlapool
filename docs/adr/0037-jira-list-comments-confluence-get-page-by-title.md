# ADR 0037：Jira `jira_list_comments` + Confluence `confluence_get_page` 支援 space+title（Issue #87）

## 狀態

Accepted — 凱撒指派後執行

## 背景

Issue #87 補上中優先批次最後兩項：

1. `jira_list_comments(issue_key)`：列出 issue 留言。
2. `confluence_get_page` 支援以 `space` + `title` 查詢，而非僅能使用 `page_id`。

## 決策

### `jira_list_comments`（讀取）

- 參數：`issue_key`（必填），沿用既有 `valid_issue_key` 驗證（PROJECT-NUMBER 格式）。
- `project` 從 `issue_key` 解析，用於 `projects` allowlist。
- 呼叫 `GET /rest/api/3/issue/{issue_key}/comment`。
- 不需要 `enable_writes`。

### `confluence_get_page` 擴充（讀取）

- 維持既有 `page_id` 查詢（向後相容）。
- 新增查詢方式：若提供 `space`（必填，用於 allowlist）與 `title`（必填），改用 `GET /wiki/api/v2/spaces/{space_id}/pages?title={title}`。
- `space_id` 為 `title` 查詢時必填，且必須為純數字。
- 若同時提供 `page_id` 與 `title`，優先使用 `page_id`（較簡單、無歧義）。
- 若 `page_id`、`title`、`space` 都未提供，或僅提供 `title` 但缺少 `space`/`space_id`，回傳 400。
- 當 `title` 查詢結果為 0 筆或超過 1 筆時，回傳 200 + `isError: true` 並說明「找不到頁面」或「找到多個頁面」，不視為上游錯誤。

### allowlist

- `jira_list_comments`：`project` 從 `issue_key` 解析，檢查 `projects` allowlist。
- `confluence_get_page`：無論使用 `page_id` 或 `title` 方式，都需要提供 `space` 以通過 `spaces` allowlist。

## 影響

- `src/mcp.rs`：新增 `jira_list_comments` 工具與分支；修改 `confluence_get_page` 分支；更新 mock server 與測試。
- `config.example.toml` 與 `README` 工具列表同步更新。
- `CHANGELOG` 加入新工具與擴充說明。

## 風險

- `confluence_get_page` 的 `title` 查詢需要 `space_id`，使用者可能混淆 `space` 與 `space_id`；文件與 schema description 需清楚區分。
- `title` 查詢可能回傳多筆，atlapool 選擇不幫使用者選擇，而是回傳錯誤訊息。
