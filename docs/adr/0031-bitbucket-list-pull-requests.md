# ADR 0031：Bitbucket `bitbucket_list_pull_requests`（Issue #76）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 atlapool 缺少 PR 列表功能。Issue #76 要求新增 MCP 讀取工具 `bitbucket_list_pull_requests`。

## 決策

### 工具參數

- `repo_slug`（必填，string）
- `state`（選填，string，預設 `OPEN`）
  - 合法值：`OPEN`、`MERGED`、`DECLINED`、`SUPERSEDED`
  - 其他值回傳 400

### 呼叫 Bitbucket REST API

- `GET /2.0/repositories/{workspace}/{repo_slug}/pullrequests?state={state}`

### 分頁處理

- 第一版只回傳 Bitbucket 回應的第一頁。
- `forward()` 偵測到 upstream body 是包含 `values` 的 object 時，自動補上 `has_more` 欄位：`true` 當且僅當回應包含 `next` 分頁 URL。
- 這讓 caller 可以判斷是否還有下一頁，而不用一次爬完全部結果。

### 權限

- 讀取工具：不經過 write-gate，不需要 `enable_writes`。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。
- 回傳統一 `CallToolResult` 格式；allowlist 拒絕回傳 HTTP 200 + `isError: true`。

## 影響

- `src/mcp.rs`：新增工具 schema、`resolve_target()` 分支、以及 `forward()` 的分頁提示邏輯。
- `src/mcp.rs` mock server：新增 `GET /repositories/{workspace}/{repo}/pullrequests` 路由。
- 文件：`README.md` 工具表、`config.example.toml` 工具列表、`CHANGELOG.md` 需同步更新。

## 風險

- 新增 `has_more` 會改變所有「列表類」upstream 回應的結構（只要有 `values` 就補 `has_more`）。現有 `bitbucket_list_branches` 與 `bitbucket_list_directory` 也會受影響，但屬於向上相容（新增欄位，不刪除既有欄位）。
- 若未來 upstream 回應也有名為 `has_more` 的欄位，會被我們覆寫；目前 Bitbucket 沒有此欄位。
