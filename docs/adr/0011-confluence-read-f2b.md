# ADR 0011：Confluence REST 代理與 MCP 讀工具（F2-b）

## 狀態
Accepted

## 背景
F2-a 已完成 Jira REST 代理與 `jira_get_issue`、`jira_create_issue` MCP 工具。atlapool 也同時需要 Confluence Cloud 的唯讀代理，讓 agent 在經過 allowlist/write-gate/audit 後，能讀取 Confluence page 內容。

## 決策

1. 新增 `src/confluence.rs`：
   - `ConfluenceClient` 結構與 `JiraClient` 一致：持有獨立的 `reqwest::Client`、base URL 與 `SecretString` token。
   - 從空 header 開始重建請求，僅注入 `Authorization: Bearer <token>`，不轉發 caller 的敏感 header。
   - `ConfluenceClient::new` 與 `request`/`send` 使用與 `JiraClient` 相同的 `UpstreamError` 錯誤類型，便於 `mcp_handler` 統一處理。
2. 在 `mcp.rs` 引入 `UpstreamClient` trait：
   - 抽出 `build_request` 與 `execute` 方法，`JiraClient` 與 `ConfluenceClient` 各自實作。
   - `mcp_handler` 在 write-gate/audit 之後，依 `params.name` 前綴選擇 Jira 或 Confluence 客戶端，再呼叫共用的 `forward()` 邏輯。
3. 新增 MCP 工具 `confluence_get_page`：
   - 參數：`space`（用於 allowlist）、`page_id`（Confluence page ID，僅接受數字字元）。
   - 對應 Confluence Cloud REST API V2：`GET /wiki/api/v2/pages/{page_id}?body-format=view`。
   - `space` 用於 `AgentConfig.authorize` 的 `space` allowlist；`project` 為 `None`。
   - `classify_tool` 會因名稱含 `get` 將其視為讀操作，所以不觸發 audit log。
4. `AppState` 加入 `confluence: Option<ConfluenceClient>`：`main.rs` 啟動時從 `AtlassianConfig` 建立（與 `JiraClient` 並行）。若未設定 `[atlassian]`，則兩者皆為 `None`。
5. 本次僅做讀端點；Confluence 寫入（create/update page）另開 Issue 處理，比照 F4 write-gate 擴充。

## 後果

- 正向：Jira 與 Confluence 共用同一套 allowlist/write-gate/audit 機制；agent 設定中的 `spaces` 對 Confluence 立即生效；header 剝除與 token 注入一致，降低攻擊面。
- 風險：Jira 與 Confluence 使用同一組 `AtlassianConfig.base_url` + token。若未來需要為兩者指定不同 domain 或不同 token，需引入 `[atlassian.jira]` 與 `[atlassian.confluence]` 子區塊；目前先比照 F2-a 最簡方案。
- 不含：Confluence 寫入端點、OAuth/Basic auth 對齊、Confluence 搜尋或列出空間等讀端點。

## 參考

- Issue #21
- PRD #1 / F2
- ADR 0003（Jira REST proxy F2-a）
- ADR 0004（per-agent allowlist F3）
- ADR 0006（write-gate F4）
- ADR 0009（audit log F7）
