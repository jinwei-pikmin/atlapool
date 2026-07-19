# ADR 0012：Atlassian Cloud REST 認證與路由對齊（Issue #25）

## 狀態
Accepted

## 背景
目前 `JiraClient` / `ConfluenceClient` 傳送 `Authorization: Bearer <token>`，並以 `atlassian.base_url` 組出 `https://<site>.atlassian.net/rest/api/3/...` 與 `https://<site>.atlassian.net/wiki/api/v2/...`。對照 Atlassian Cloud REST API 官方規範，真實環境會因認證方式與路由錯誤而失敗。

## 決策

1. 認證改為 **Basic Auth**：
   - `Authorization: Basic <base64(email:token)>`。
   - 新增 `AtlassianConfig.email` 欄位，與既有 `token` 配對。
   - `email` 與 `token` 都支援 secret reference（`env:` / `aws:` / `gcp:`）。

2. 路由改走 `api.atlassian.com` 閘道：
   - 新增 `AtlassianConfig.cloud_id` 欄位。
   - `JiraClient` 使用 `https://api.atlassian.com/ex/jira/{cloud_id}` 作為 base URL。
   - `ConfluenceClient` 使用 `https://api.atlassian.com/ex/confluence/{cloud_id}` 作為 base URL。
   - 當 `cloud_id` 未設定時，退回到 `base_url`（方便 mock server 或 on-prem 測試）。

3. `ConfluenceClient` 讀 page 改為 Confluence Cloud V1 Content API：
   - 路徑為 `/wiki/rest/api/content/{page_id}?expand=body.storage`。
   - 避免 V2 API 在部分 token scope 下回 401 的問題。

4. `base_url` 保留但功能改變：
   - 主要用於人類可讀連結與查詢 `cloud_id`（`https://<site>.atlassian.net/_edge/tenant_info`）。
   - 不再直接用於生產環境的 REST 呼叫；生產呼叫以 `cloud_id` 為準。

5. 既有機制維持不變：
   - Header 從空集合重建，不轉發 caller 敏感 header。
   - 仍由 server 注入 `Authorization`；caller 的 API token 不離開 atlapool。
   - allowlist、write-gate、audit log 邏輯不變。

## 後果

- 正向：與 Atlassian Cloud REST API 官方規範一致；使用 Basic Auth 與 `api.atlassian.com` 閘道後，Jira 與 Confluence 呼叫在真實環境中可成功。
- 風險：`cloud_id` 成為必填欄位（生產環境）。config.example.toml 與 README 需同步更新，避免使用者沿用舊的 `base_url` + token 設定。
- 不含：ADF 格式驗證、Confluence V2 API、rate limit / retry、page-level 細粒度 allowlist。

## 參考

- Issue #25
- Atlassian 官方文件：Jira Cloud REST API、Confluence Cloud REST API
- ADR 0003（Jira REST proxy F2-a）
- ADR 0011（Confluence read proxy F2-b）
