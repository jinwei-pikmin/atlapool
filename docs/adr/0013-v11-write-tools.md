# ADR 0013：v1.1 寫入工具擴充（Issue #29）

## 狀態
Accepted

## 背景
v1 已提供 Jira 讀取（`jira_get_issue`）、Jira 建立 issue（`jira_create_issue`）、Confluence 讀取（`confluence_get_page`）。agent 需要能回應與補充資訊：在 Jira issue 下留言、在 Confluence 建立或更新 page。Issue #29 PRD 定義三支新寫入工具。

## 決策

1. 分兩階段實作，先 `jira_add_comment`（不受 Confluence 環境阻塞），後 `confluence_create_page` / `confluence_update_page`。
2. 每支工具沿用既有三大機制：
   - F3 per-agent allowlist（project / space 維度）
   - F4 write-gate（`enable_writes = true`）
   - F7 fail-closed audit log（audit 寫入失敗則不觸發上游）
3. `jira_add_comment`：
   - 對應 `POST /rest/api/3/issue/{issue_key}/comment`（Issue #25 後走 `api.atlassian.com` 閘道）。
   - 必填參數：`issue_key`（allowlist project 解析）、`body`（ADF，原樣透傳）。
   - 發往 Jira 的 JSON body 為 `{"body": <caller-body>}`，`issue_key` 不會被帶入上游 payload。
   - allowlist 維度：`projects`（由 `issue_key` split `-` 取 project）。
   - `classify_tool` 因名稱不含 `get/list/search/read` 視為 Write，觸發 write-gate 與 audit。
4. `confluence_create_page` / `confluence_update_page`：
   - 走 `api.atlassian.com/ex/confluence/{cloud_id}/wiki/rest/api/content`。
   - allowlist 維度：`spaces`（`space_key`），**page-level 細粒度不做**（至尊裁定 space 層已足夠）。
   - `confluence_update_page` 需要呼叫方提供 `version`（Confluence 樂觀鎖）。
   - 409 conflict 直接透傳，不特殊處理。
5. 文件同步：README MCP 工具清單、config.example.toml 註解、測試逐步更新。

## 後果
- 正向：agent 能在被授權 project/space 內留言或產出/修訂文件，且同樣受 allowlist/write-gate/audit 保護。
- 風險：`jira_add_comment` 的 `body` 與 Confluence 的 `body` 都原樣透傳，呼叫方需自行組裝合法 ADF / storage 格式，錯誤格式會直接由 Atlassian 回 400。
- 不含：ADF/storage 格式驗證、page-level allowlist、rate limit / retry、Jira issue 欄位更新、Confluence page 刪除。

## 參考
- Issue #29
- ADR 0003（Jira REST proxy）
- ADR 0011（Confluence read proxy）
- ADR 0012（Atlassian auth & cloud_id gateway）
