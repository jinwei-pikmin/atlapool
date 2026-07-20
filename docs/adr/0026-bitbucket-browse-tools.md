# ADR 0026：Bitbucket browse tools（Issue #65 / #66）

## 狀態

Accepted — 凱撒指派後執行

## 背景

金霸提出 Luigi agent 不 clone repo，改用 MCP 工具瀏覽/編輯程式碼（比照 ghpool 做法）。
為此需要先能列出分支、瀏覽目錄、讀取檔案內容，才能決定後續要讀哪個分支、哪個檔案，並與既有的 `bitbucket_create_branch`/`bitbucket_create_commit`/`bitbucket_create_pull_request` 寫工具銜接。

Issue #65 與 #66 分別規格化這三個讀取工具，建議在同一個 feature branch 實作以共享 repo_slug 驗證與 Bitbucket client 路徑。

## 決策

新增三支 MCP 讀取工具：

- `bitbucket_list_branches`
- `bitbucket_list_directory`
- `bitbucket_get_file_content`

全部沿用現有 `repo_slug` 字元白名單驗證，由 `bitbucket_workspaces` 與 `bitbucket_repos` allowlist 控管；皆為 read 工具，不需要 `enable_writes`，不寫 audit。

### `bitbucket_list_branches`

- `GET /repositories/{workspace}/{repo_slug}/refs/branches`
- 回傳 upstream 原始 JSON（`values[].name` + `values[].target.hash`）
- 分頁遍歷留給後續；本次至少回傳第一頁

### `bitbucket_list_directory` 與 `bitbucket_get_file_content`

兩者都呼叫 Bitbucket Source API：

- `GET /repositories/{workspace}/{repo_slug}/src/{ref}/{path}`
- `path` 選填，預設 repo 根目錄
- `ref` 選填，預設 repo 預設分支

當 `ref` 省略時，atlapool 先呼叫 `GET /repositories/{workspace}/{repo_slug}` 取得 `mainbranch.name`，再用該分支名稱組出 Source API URL。這會多一次 upstream read，但確保 `path` 存在時也能正確命中預設分支。

#### 路徑與 ref 驗證

- `repo_slug`：沿用現有白名單（ASCII alphanumeric + `_`、`.`、`-`）
- `path`：
  - 選填（`bitbucket_list_directory`），必填（`bitbucket_get_file_content`）
  - 不得以 `/` 開頭
  - 不得包含 `..` 路徑段
  - 各段以 URL path-segment 編碼後組成 URL
- `ref`：
  - 選填
  - 若提供則視為單一 path segment，不得以 `/` 開頭或包含 `/`
  - 原因：Bitbucket `src/{ref}/{path}` endpoint 無法區分支名稱中的 `/` 與路徑分隔符；分支名稱含 `/` 時，caller 應先用 `bitbucket_list_branches` 取得 commit hash，再以 hash 作為 `ref`

#### 目錄 vs 檔案

- `bitbucket_list_directory`：在組出的 URL 結尾加上 `/`（目錄 trailing slash），讓 Bitbucket 回傳 paginated tree entry
- `bitbucket_get_file_content`：不附加 trailing slash；Bitbucket 回傳檔案原始內容，atlapool 以 JSON string 包裝後回傳

## 影響

- `mcp.rs` 新增三個 `resolve_target` 分支與一個 `resolve_bitbucket_default_branch` 輔助函式
- `ToolTarget` 需新增 `resolve_ref` staging field，讓 handler 在授權後才做預設分支查詢
- mock Bitbucket server 需新增 `GET /refs/branches`、`GET /src/{*path}` 與 repo `mainbranch` 回應
- README 與 `config.example.toml` 工具表需更新

## 風險

- 預設分支解析增加一次 upstream call，失敗時回傳 502/400
- `ref` 含 `/` 不支援；caller 必須自行轉換為 commit hash
- `path` 中的特殊字元依賴 URL path-segment 編碼；若編碼不當可能導致 404
