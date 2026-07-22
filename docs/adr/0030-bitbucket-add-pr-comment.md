# ADR 0030：Bitbucket `bitbucket_add_pull_request_comment`（Issue #75）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 atlapool 缺少 PR 留言工具。Issue #75 要求新增 MCP 寫入工具 `bitbucket_add_pull_request_comment`。

## 決策

### 工具參數

- `repo_slug`（必填，string）
- `pull_request_id`（必填，string）：非空且純數字，沿用既有驗證。
- `content`（必填，string）：留言內容，不設長度限制，由 Bitbucket API 自行拒絕超長內容。

### 呼叫 Bitbucket REST API

- `POST /2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/comments`
- Request body：`{"content":{"raw": <content>}}`

### 權限與審計

- 寫入工具：走 write-gate（`enable_writes=true`）與 fail-closed audit（`attempt` + `result`）。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。
- 回傳統一 `CallToolResult` 格式；拒絕時 HTTP 200 + `isError: true`。

### 資安與可追溯性限制

- **Audit log 不落地留言全文**：`audit` 記錄的欄位與現有一致（`agent_id`、`tool`、`target`、`result` 等），`target` 只會是 workspace/repo 維度，不會包含 `content`。
- **身份可追溯性限制**：所有 upstream 請求都使用同一組 Bitbucket Service Account token，因此留言在 Bitbucket 端會顯示為同一個機器人帳號，**無法**從 Bitbucket 原生紀錄區分是哪個 agent 發出的。事後追溯必須依賴 atlapool 自己的 audit log（包含 `agent_id`）。

## 影響

- `src/mcp.rs`：新增 `bitbucket_add_pull_request_comment` 工具 schema 與 `resolve_target()` 分支。
- `src/mcp.rs` mock server：新增 `POST /pullrequests/{pr_id}/comments` 路由。
- 文件：README、config.example.toml、CHANGELOG 需同步更新，並明確記載身份可追溯性限制。

## 風險

- `content` 完全透傳，若包含 `@mention` 會觸發真實通知；這是預期功能，不額外過濾。
- 留言內容可能不小心包含敏感資訊，但 audit log 不記錄全文，降低二次外洩風險。
- Bitbucket API 的內容長度上限由上游決定；atlapool 不預先限制，避免與上游規則不同步。
