# ADR 0015：Bitbucket read tools（Issue #38）

## 狀態
Accepted

## 背景
atlapool 目前支援 Jira 與 Confluence。Issue #38 要求比照 ghpool 的 REST proxy 模式擴充 Bitbucket，只轉發 API 呼叫，不執行任何 shell git 指令。先實作兩支讀取工具：`bitbucket_get_repo`、`bitbucket_get_pull_request`。

## 決策

1. 新增 `src/bitbucket.rs`：`BitbucketClient` 比照 `ConfluenceClient` 結構：
   - base URL：`https://api.bitbucket.org/2.0`
   - server 注入 `Authorization: Bearer <token>`
   - 不帶 caller header；request 從空 header 開始
2. `Config` 新增 `[bitbucket]` 區塊：
   - `workspace`：Bitbucket workspace slug
   - `token`：`SecretString`，支援 env / AWS / GCP secret reference（由 `LazyMultiBackend` 解析）
3. `AgentConfig` 新增兩個 allowlist 欄位：
   - `bitbucket_workspaces`
   - `bitbucket_repos`
   - 行為比照 `projects` / `spaces`，使用相同 glob `*` 語意。
4. `ToolTarget` 擴充 `workspace`、`repo` 兩個維度；`AgentConfig::authorize` 同步擴充：
   - 若 request 帶 `project`、`space`、`workspace`、`repo` 中任何非空值，該值必須符合對應 allowlist；全部非空維度都符合才允許。
   - 維持 deny-by-default：工具名稱不在 `tools` 清單內，或所有維度皆空，皆拒絕。
5. MCP 路由：
   - `bitbucket_` 開頭 → `BitbucketClient`
   - `confluence_` 開頭 → `ConfluenceClient`
   - 其餘 → `JiraClient`
6. 工具定義：
   - `bitbucket_get_repo`：讀取 repository 資訊
     - 必填：`repo_slug`
     - 路徑：`/repositories/{workspace}/{repo_slug}`
   - `bitbucket_get_pull_request`：讀取單一 PR
     - 必填：`repo_slug`、`pull_request_id`
     - 路徑：`/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}`
   - workspace 從 `[bitbucket]` config 注入，不允許 caller 任意指定。
7. 這兩支工具名稱含 `get`，`classify_tool` 視為 Read，不經 write-gate / audit。
8. 文件同步：README MCP 工具表、config.example.toml、單元/E2E 測試。

## 後果
- 正向：atlapool 可用同一套 allowlist + secret proxy 模式讀取 Bitbucket repo/PR。
- 風險：workspace 固定在 config，單一 agent 只能存取一個 workspace；未來若要 per-call workspace 需再改設計。
- 不含：Bitbucket 寫入工具、webhook、OAuth 授權流程、分頁處理、repo 層級以外的 allowlist。

## 參考
- Issue #38
- `src/confluence.rs`
- `src/upstream.rs`
- `src/secrets.rs`
