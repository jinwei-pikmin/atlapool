# ADR 0033：Bitbucket `bitbucket_get_pipeline_status`（Issue #78）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊 CI gate 流程，發現 atlapool 缺少 Bitbucket Pipelines 狀態查詢能力。Issue #78 要求新增 MCP 讀取工具 `bitbucket_get_pipeline_status`。

## 決策

### 工具參數

- `repo_slug`（必填，string）
- `branch`（必填，string）：要查詢的分支名稱。

### 呼叫 Bitbucket REST API

- `GET /2.0/repositories/{workspace}/{repo_slug}/pipelines/?sort=-created_on&target.branch={branch}&pagelen=1`
- `branch` 先做 `valid_branch_name` 驗證，再以 percent-encoding 編碼後放入 query string。

### OAuth scope 需求

- `bitbucket_get_pipeline_status` 只需要 Bitbucket OAuth scope `pipeline`（唯讀）。
- `pipeline:write` 不需要，atlapool 也不會觸發或修改 pipeline。
- 使用 App password 或 API token 時，對應權限為 `read:pipeline:bitbucket`。

### Bitbucket Pipelines 狀態語意

Bitbucket 回傳兩個欄位：`state` 與 `result`。

- `state` 可能值：`PENDING`、`IN_PROGRESS`、`RUNNING`、`PAUSED`、`COMPLETED`。
- 當 `state` 為 `COMPLETED` 時，`result` 可能為 `SUCCESSFUL`、`FAILED`、`ERROR`、`STOPPED`、`EXPIRED`。

`forward()` 會把這兩個欄位正規化為單一 `normalized_status`：

| `state` | `result` | `normalized_status` |
|---|---|---|
| `PENDING`/`IN_PROGRESS`/`RUNNING`/`PAUSED` | any | `running` |
| `COMPLETED` | `SUCCESSFUL` | `passed` |
| `COMPLETED` | `FAILED`/`ERROR`/`STOPPED`/`EXPIRED` | `failed` |
| 其他 | 其他 | `unknown` |

### 沒有 pipeline 紀錄時的行為

- 若 upstream 回傳 200 但 `values` 為空或不存在，或回傳 404，視為「沒有設定 Pipelines 或沒有執行紀錄」，回傳 `{"normalized_status":"unknown","message":"..."}` 且 `isError: false`。
- 其他非 2xx/404 錯誤仍回傳 `isError: true`。

### 權限

- 讀取工具：不經 write-gate，不需要 `enable_writes`。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。

## 影響

- `src/mcp.rs`：新增工具 schema、`resolve_target()` 分支、pipeline 狀態正規化函式、mock server pipelines endpoint。
- `forward()` 增加 `bitbucket_get_pipeline_status` 的特殊處理邏輯，將 404/空結果視為「無紀錄」而非錯誤。
- 文件：README 狀態對照表、config.example.toml 工具列表、CHANGELOG 同步更新。

## 風險

- 將 pipeline 相關 404 視為「無紀錄」可能掩蓋 repo 不存在的問題，但 allowlist 之前已經過 workspace/repo 維度授權，此情況風險低。
- 狀態正規化無法覆蓋 Bitbucket 未來新增狀態值，將落入 `unknown`。
