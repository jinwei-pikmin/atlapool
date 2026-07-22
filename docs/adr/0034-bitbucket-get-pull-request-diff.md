# ADR 0034：Bitbucket `bitbucket_get_pull_request_diff`（Issue #84）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸對標團隊實際工作流程，發現 atlapool 已有 diffstat（Issue #77），但缺少 raw diff，無法進行逐行 code review。Issue #84 要求新增 MCP 讀取工具 `bitbucket_get_pull_request_diff`。

raw diff 可能意外包含密鑰、token 或個資，因此需要基本 redaction、截斷與 binary 處理。

## 決策

### 工具參數

- `repo_slug`（必填，string）
- `pull_request_id`（必填，string）：數字 PR ID。
- `max_lines`（選填，number）：預設 2000，超過即截斷。

### 呼叫 Bitbucket REST API

- `GET /2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/diff`
- `pull_request_id` 沿用既有驗證（非空且全為數字）。
- 此 endpoint 可能回傳 302 redirect 到 CDN；reqwest 預設會 follow redirect，並在 cross-host 時自動移除 `Authorization` header。

### 回應處理

`forward()` 會針對 `bitbucket_get_pull_request_diff` 做特殊處理：

1. 以 `bytes()` 讀取 upstream body。
2. 若無法解碼為 UTF-8，視為 binary，回傳 `{"diff": "[binary file, diff not shown]", "binary": true}`。
3. 能解碼為 UTF-8 時，先對常見密鑰 pattern 做 regex redaction，命中處以 `[REDACTED]` 取代。
4. 再按 `max_lines` 截斷，若截斷則回傳 `{"diff": "...", "truncated": true, "total_lines": N}`。
5. 回傳 JSON 物件，由 `wrap_calltool_result` 包裝為 `CallToolResult`。

### Redaction 規則

- AWS access key：`AKIA[0-9A-Z]{16}`
- OpenAI-style key：`sk-[a-zA-Z0-9]{20,}`
- Bearer token：`Bearer [a-zA-Z0-9._-]{20,}`

> 注意：此 redaction 僅為基本防護，**不是完整 DLP**。機敏資料仍可能以其他格式存在，使用者與審查者仍需自行警覺。

### 權限

- 讀取工具：不經 write-gate，不需要 `enable_writes`。
- `bitbucket_workspaces` / `bitbucket_repos` allowlist 與其他 Bitbucket 工具一致。
- 需要 `pullrequest:read` scope。

## 影響

- `src/mcp.rs`：新增工具 schema、`resolve_target()` 分支、`forward()` diff 特殊處理、redaction helper、mock server diff endpoint。
- `Cargo.toml`：新增 `regex` dependency。
- 文件：README redaction 聲明與工具列表、config.example.toml 工具列表、CHANGELOG 同步更新。

## 風險

- Redaction 無法涵蓋所有密鑰格式，README 需明確聲明限制。
- 截斷後的 diff 無法完整 review，需提示使用者取得完整內容的其他方式。
- Binary 檔案無法顯示 diff，需標記清楚。
