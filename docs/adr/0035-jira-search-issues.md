# ADR 0035：Jira `jira_search_issues`（Issue #85）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 agent 只能查已知 issue key，無法主動找票。Issue #85 要求新增 MCP 讀取工具 `jira_search_issues`。

JQL 本身可以繞過 `projects` allowlist（例如 `project = OTHER`），因此必須在 atlapool 層強制限制查詢範圍。

## 決策

### 工具參數

- `project`（必填，string）：單一 Jira project key，先過 `projects` allowlist。
- `jql_filter`（選填，string）：額外 JQL 條件，例如 `status = "In Progress"`。
- `max_results`（選填，number）：預設 50，上限 100；超過上限時直接夾住為 100。

### 強制 project 前綴

atlapool 在 `resolve_target()` 組出最終 JQL，強制前綴為：

```
project = "{project}" AND ({jql_filter})
```

若 `jql_filter` 為空，最終 JQL 為：

```
project = "{project}"
```

這確保上游 JQL 一定以 project 範圍開頭，`jql_filter` 無法覆寫或繞過 allowlist。

### 關鍵字黑名單

為防止 `jql_filter` 內夾帶 `project = ...` 或 `projectKey = ...` 試圖改變範圍，`resolve_target()` 先對 `jql_filter` 做大小寫不敏感的字級比對：若包含 `project` 或 `projectkey` 這兩個 token，直接 400 拒絕，不送上游。

實作使用 regex `\b(project|projectkey)\b`（case-insensitive）。這不是完整 JQL 語法解析，但能以較低成本堵住主要繞過路徑。

> 限制：此黑名單仍可能誤判字面值（如 `text ~ "project"`），或漏過高級編碼繞過。使用者輸入仍應視為不可信。

### 呼叫 Jira REST API

- `POST /rest/api/3/search`
- Body: `{"jql": "...", "maxResults": N}`
- 讀取工具，不需要 `enable_writes`。

### allowlist

- `project` 填入 `ToolTarget.project`，由既有 `AgentConfig::authorize()` 檢查 `projects` allowlist。
- 未通過 allowlist 時回傳 200 + `isError: true` 的政策錯誤（沿用既有模式，非 403）。

### max_results 上限

- `max_results` 在 `resolve_target()` 階段即 clamp 為 `min(max_results, 100)`，避免單次回應過大。

## 影響

- `src/mcp.rs`：新增工具 schema、`resolve_target()` 分支、JQL 組合邏輯、黑名單 regex、mock Jira search endpoint、測試。
- `src/agents.rs` 與 `src/upstream.rs` 原則上無需改動：project allowlist 與 JSON body 路徑已存在。
- 文件：README 安全設計說明、`config.example.toml` 工具列表、CHANGELOG 同步更新。

## 風險

- 黑名單非完整 JQL 解析，存在繞過可能；README 與 ADR 需明確聲明。
- `jql_filter` 黑名單可能誤判合法字面值，凱撒/加圖可視使用回饋調整。
- `max_results` 上限固定為 100，未來如需更大結果集可改為分頁。
