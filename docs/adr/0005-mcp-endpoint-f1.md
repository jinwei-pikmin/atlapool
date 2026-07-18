# ADR 0005：對外 MCP endpoint（F1）

## 狀態
Accepted

## 背景
atlapool 已具備 secret 解析（F5-a）、上游 REST 代理骨架（F2-a）、per-agent identity + allowlist（F3）。F1 要把三塊地基串成可對外服務的 MCP Streamable-HTTP endpoint，並完成第一次端到端驗證。

## 決策

1. 新增 `POST /mcp` endpoint，處理 JSON-RPC `tools/call` 請求：
   - 讀取 `X-Atlapool-Key` header；缺失或空 → 401。
   - 用 F3 `find_agent()` 以**常數時間**比對 key（`subtle::constant_time_eq`）；找不到 → 401。
   - 解析 `params.name`（tool）與 `params.arguments`。
   - 根據 tool/arguments 解析出 `project` 與 `space`；無法解析且 tool 需要時 → 403（由 `authorize` 實現 deny-if-unresolvable）。
   - 呼叫 F3 `AgentConfig::authorize(tool, project, space)`；失敗 → 403。
   - 用 F2-a `JiraClient` 組出上游 `Authorization: Bearer <token>` 請求，**丟棄 client 原始 `Authorization`/`Cookie`**。
   - 回傳 JSON-RPC 2.0 回應（`result` 或 `error`）。
2. 常數時間 key 比對：
   - 依賴 `subtle` crate，`find_agent` 以 `subtle::constant_time_eq` 比較 key bytes；不在長度差異處提前 return。
3. Tool 對應與 project/space 解析：
   - F1 先實作 `jira_get_issue`：從 `issue_key`（如 `PROJ-123`）split 出 `project`。
   - 上游 path：`GET /rest/api/3/issue/{issue_key}`。
   - Confluence tool 留給後續 Issue；未知 tool 由 `authorize()` default-deny 處理。
4. `AppState` 加入 `jira: Option<JiraClient>`，main 在載入 config 後初始化。
5. 單元/整合測試：
   - 啟動 mock Jira server（axum），驗證 `/mcp` 401/403/200 路徑。
   - 200 時檢查 mock 收到的 header 只含 server 注入的 `Authorization`，不含 client `X-Atlapool-Key`/`Cookie`。

## 後果

- 正向：第一次完整鏈路驗證（key → agent → authorize → upstream token injection），資安核心邏輯自研可控。
- 風險：F1 仍用 F2-a 的 `Bearer <token>` 煙霧方案；真實 Atlassian Basic/OAuth 認證待 F2-b。
- 不含：Confluence tool、MCP SSE/session、write gate、審計。

## 參考

- Issue #9
- PRD #1 / F1、F2、F3
- ghpool `src/mcp.rs` 參考其 header 轉發與 JSON-RPC 處理
