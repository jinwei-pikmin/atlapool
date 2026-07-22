# ADR 0032：Bitbucket `bitbucket_list_pull_request_changes`（diffstat，Issue #77）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 atlapool 缺少 PR 變更查看工具。經維特魯威與加圖討論，本輪先做 diffstat（檔名 + 統計），不做 raw diff，因為 raw diff 可能洩露密鑰或個資，需要額外截斷與 redact 機制，複雜度過高。

## 決策

### 工具參數

- `repo_slug`（必填，string）
- `pull_request_id`（必填，string）：非空且純數字，沿用既有驗證。

### 呼叫 Bitbucket REST API

- `GET /2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/diffstat`

### 回傳內容

- 直接回傳 upstream 的 diffstat JSON，包含 `values` 陣列。
- 每個元素預期包含 `status`（added/modified/removed）、檔案路徑（`old.path` / `new.path`）、行數統計（`lines_added` / `lines_removed`）等欄位。
- **不回傳實際程式碼內容**。此工具無法用於「查看改了什麼程式碼」的完整 code review，只能用於「哪些檔案改了多少行」的摘要。

### 分頁

- 與 `bitbucket_list_pull_requests` 一致，`forward()` 會自動補上 `has_more` 提示。

### 權限

- 讀取工具：不經 write-gate，不需要 `enable_writes`。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。

## 影響

- `src/mcp.rs`：新增工具 schema、`resolve_target()` 分支、mock server diffstat endpoint。
- 文件：README 工具表需明確標註「diffstat，非 raw diff」；config.example.toml 與 CHANGELOG 同步更新。

## 風險

- 使用者可能誤以為此工具能拿到 raw diff，因此 README 與 schema description 必須清楚說明只回傳統計。
- raw diff 的敏感資訊問題本輪未解決，已在 Issue 中標記為下一輪工作。
