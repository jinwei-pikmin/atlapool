# ADR 0036：Confluence `confluence_list_pages` + `confluence_delete_page`（Issue #86）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 atlapool 缺少 Confluence 列出頁面與刪除頁面能力。Issue #86 要求新增兩支 MCP 工具：`confluence_list_pages` 與 `confluence_delete_page`。

## 決策

### `confluence_list_pages`（讀取）

- 參數：
  - `space`（必填，string）：用於 `spaces` allowlist 比對。
  - `space_id`（必填，string）：數字 space ID，實際查詢用。
- 呼叫 `GET /wiki/api/v2/spaces/{space_id}/pages`。
- 分頁：第一版只回傳第一頁，並由 `forward()` 的 `has_more` 機制在 upstream response 含有 `next` 時加上 `has_more: true`。
- 不需要 `enable_writes`。

### `confluence_delete_page`（寫入）

- 參數：
  - `space`（必填，string）：用於 `spaces` allowlist 比對。
  - `page_id`（必填，string）：數字 page ID。
- 呼叫 `DELETE /wiki/api/v2/pages/{page_id}`。
- 沿用既有 `page_id` 驗證（非空且全為數字）。
- 走 write-gate 與 fail-closed audit（與其他刪除工具一致）。

### allowlist

- `space` 填入 `ToolTarget.space`，由 `AgentConfig::authorize()` 檢查 `spaces` allowlist。
- 未通過 allowlist 時回傳 200 + `isError: true` 的政策錯誤（非 403）。

## 影響

- `src/mcp.rs`：新增兩支工具 schema、`resolve_target()` 分支、mock Confluence server endpoint、測試。
- 文件：README 工具列表、config.example.toml 工具列表、CHANGELOG 同步更新。

## 風險

- `confluence_list_pages` 第一版不支援翻頁參數，未來需要時再加 `cursor`/`limit`。
- `confluence_delete_page` 為破壞性寫入，須確保 write-gate 與 audit 正確觸發。
