# ADR 0029：Bitbucket `bitbucket_decline_pull_request` 與 `bitbucket_delete_branch`（Issue #74）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸要求對標團隊實際工作流程，補齊 atlapool 在 Bitbucket PR 操作上的缺口。
Issue #74 要求一次實作兩個高優先、低複雜度的寫入工具：

- `bitbucket_decline_pull_request`：關閉 PR 但不合併。
- `bitbucket_delete_branch`：刪除已合併或已棄用的分支。

## 決策

### `bitbucket_decline_pull_request`

- 參數：`repo_slug`（必填）、`pull_request_id`（必填，string，非空且純數字）。
- 呼叫 Bitbucket Cloud `POST /2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/decline`，無 request body。
- `pull_request_id` 驗證沿用 `bitbucket_get_pull_request` / `bitbucket_merge_pull_request` 的既有規則。

### `bitbucket_delete_branch`

- 參數：`repo_slug`（必填）、`branch_name`（必填，string）。
- 呼叫 Bitbucket Cloud `DELETE /2.0/repositories/{workspace}/{repo_slug}/refs/branches/{branch_name}`。
- `branch_name` 驗證沿用 `.`/`..` 與前導 `/` 拒絕的邏輯；與 `valid_ref_name` 的差異在於**允許 `/`**，因為分支名如 `feature/x` 是常見且合法的，只需將整個分支名作為單一路徑 segment 進行 percent-encode。
- 使用 `encode_path_segment` 對 `branch_name` 編碼，避免特殊字元破壞 URL。

### 權限與審計

- 兩者皆為寫入工具：走 write-gate（`enable_writes=true`）與 fail-closed audit（`attempt` + `result`）。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。
- 回傳格式沿用已統一的 `CallToolResult`；write-gate / allowlist 拒絕回傳 HTTP 200 + `isError: true`。

## 影響

- `src/mcp.rs`：在 `tools()` 靜態工具表新增兩個 schema；在 `resolve_target()` 新增兩個分支。
- `src/mcp.rs` mock Bitbucket server：新增 `POST .../decline` 與 `DELETE .../refs/branches/{name}` 路由。
- 文件：`README.md` 工具表、config.example.toml 工具列表、`CHANGELOG.md` 需同步更新。

## 風險

- `bitbucket_delete_branch` 的 `branch_name` 若包含 `/`，需正確視為單一路徑 segment 編碼；若直接拼進 URL 會被解析為多層路徑而 404。
- `DELETE` 請求無 body；Bitbucket 對 protected branch 會回 400，屬正常上游錯誤，會包成 `CallToolResult{isError: true}`。
- `decline` 對已合併或已 declined 的 PR 會回非 2xx，同樣以 `isError: true` 呈現。
