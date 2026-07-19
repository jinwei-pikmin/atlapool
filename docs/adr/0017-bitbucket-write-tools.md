# ADR 0017：Bitbucket 寫入工具（Issue #39 / #45）

## 狀態
Accepted

## 背景
#38 已引入 Bitbucket 讀取工具（`bitbucket_get_repo`、`bitbucket_get_pull_request`）。#39 與 #45 要求新增 Bitbucket 寫入工具：建立 branch、commit、pull request，以及建立新 repository。所有寫入工具必須沿用現有的 write-gate 與 fail-closed audit。

## 決策

1. 新增三支 MCP 寫入工具：
   - `bitbucket_create_branch`
   - `bitbucket_create_commit`
   - `bitbucket_create_pull_request`
   - 以及後續 #45 的 `bitbucket_create_repo`
2. 工具名稱皆不含 `get/list/search/read`，`classify_tool` 視為 Write，因此：
   - 需要 `agent.enable_writes = true`
   - 需要 `audit.path` 可寫，且寫入失敗時拒絕請求（fail-closed）
3. Allowlist 維度沿用 `bitbucket_workspaces` 與 `bitbucket_repos`。
4. `repo_slug` 驗證沿用 #42 的字元白名單 `[a-zA-Z0-9_.-]`，不得包含 `/`，防止路徑穿越。
5. 實作細節：
   - `bitbucket_create_branch`：`POST /repositories/{workspace}/{repo_slug}/refs/branches`
     - body: `{"name": "<branch_name>", "target": {"hash": "<commit_hash>"}}`
   - `bitbucket_create_commit`：`POST /repositories/{workspace}/{repo_slug}/src`
     - 使用 `application/x-www-form-urlencoded`（Bitbucket source API 所要求）
     - body 由 caller 提供的 JSON 轉換為 form fields：`message`、`branch`、`parents`，以及 `files` map（key=path, value=content）
     - 禁止使用本地 `git` 指令
   - `bitbucket_create_pull_request`：`POST /repositories/{workspace}/{repo_slug}/pullrequests`
     - body: `{"title": "...", "source": {"branch": {"name": "..."}}, "destination": {"branch": {"name": "..."}}}`，`description` 可選
   - `bitbucket_create_repo`（#45）：`POST /repositories/{workspace}/{repo_slug}`
     - body: `{"is_private": true}` 為預設，可由 caller 覆寫
6. 為支援 `bitbucket_create_commit` 的 form body，擴充 `UpstreamClient`：
   - `ToolTarget.body` 改為 `RequestBody` enum：`Json(Value)`、`Form(Vec<(String, String)>)` 或 `None`
   - `JiraClient` 與 `ConfluenceClient` 繼續使用 JSON body
   - `BitbucketClient` 根據 `RequestBody` 變體設定 `Content-Type`
7. 文件同步：README MCP 工具表、config.example.toml 工具與 allowlist 註解。

## 後果
- 正向：agent 可在被授權 workspace/repo 內建立 branch、commit、PR 與 repo，且受 write-gate/audit 保護。
- 風險：`bitbucket_create_commit` 的 form body 結構與 Bitbucket API 緊耦合；caller 需了解 `files` map 格式。
- 不含：本地 git 操作、webhook、OAuth 流程、repo/branch 層級以外的 allowlist。

## 參考
- Issue #39
- Issue #45
- ADR 0015（Bitbucket read tools）
- `src/bitbucket.rs`、`src/audit.rs`、`src/mcp.rs`
