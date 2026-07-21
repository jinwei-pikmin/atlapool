# ADR 0028：Bitbucket `bitbucket_merge_pull_request` 工具（Issue #72）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸稽查發現 atlapool 已提供 `bitbucket_create_pull_request`，但缺少合併 PR 的能力；agent 只能開 PR，無法自己完成合併流程。

Issue #72 要求新增 MCP 寫入工具 `bitbucket_merge_pull_request`。

## 決策

### 工具參數

- `repo_slug`（必填，string）：儲存庫 slug。
- `pull_request_id`（必填，string）：PR ID，沿用 `bitbucket_get_pull_request` 的驗證規則——非空且必須為純數字；否則回傳 400。
- `close_source_branch`（選填，boolean，預設 `true`）：合併後是否關閉來源分支。

### Merge 策略

- 固定為 `merge_commit`。
- 不開放 `squash` / `fast_forward` 選項，避免一次給 agent 過多選擇；未來需要時再擴充。

### 呼叫 Bitbucket REST API

- `POST /2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/merge`
- request body：
  ```json
  {
    "merge_strategy": "merge_commit",
    "close_source_branch": <true|false>
  }
  ```

### 權限與審計

- 走 write-gate：`enable_writes=true` 才允許呼叫。
- 走 fail-closed audit：呼叫前寫 `attempt`，呼叫後寫 `result`。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。
- 工具回傳統一使用 `CallToolResult`（與 PR #71 決議一致）：2xx 回應 `isError: false`；write-gate 拒絕等錯誤 `isError: true` 並保持 HTTP 200。

## 影響

- `src/mcp.rs`：在 `tools()` 靜態工具表新增 `bitbucket_merge_pull_request` 與 `inputSchema`；在 `resolve_target()` 新增分支處理。
- `src/bitbucket.rs`：可選新增輔助方法組合請求（與 `create_pull_request_request` 一致風格）。
- 測試：更新 mock Bitbucket server 以支援 `/merge` endpoint；新增 enable/disable writes、非法 `pull_request_id`、audit 兩筆、allowlist 驗證的測試。
- 文件：README 工具表與範例、config.example.toml 工具列表、`CHANGELOG.md` 需同步更新。

## 風險

- `pull_request_id` 沿用「純數字」驗證，若 Bitbucket 未來支援非數字 PR ID 需放寬。
- 僅支援 `merge_commit`；若用戶需要 squash 或 fast-forward，必須再開一次擴充。
- `pullrequest:write` scope 已涵蓋合併動作，無需新 scope，但文件需確認說明一致。
