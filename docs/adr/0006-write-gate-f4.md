# ADR 0006：寫入閘門（F4）

## 狀態
Accepted

## 背景
MCP endpoint（F1）已串起身分、allowlist 與上游代理。必須新增 write gate，避免持有 read-only key 的 agent 意外或惡意執行寫入操作。

## 決策

1. `AgentConfig` 新增 `enable_writes: bool`，預設 `false`。
2. 工具名稱分類：
   - `get_*`、`list_*`、`search_*` → `ToolKind::Read`。
   - 其他（含未知工具名）→ `ToolKind::Write`。
3. `/mcp` handler 流程：
   - 解析 `params` → `resolve_target`（未知/未支援工具明確回 `Err("unsupported tool")`）。
   - `AgentConfig::authorize(tool, project, space)`。
   - 若 `classify_tool(tool) == Write` 且 `agent.enable_writes == false` → 403。
   - 其餘放行，由 `JiraClient` 發出上游請求。
4. `resolve_target` 對未知工具不再回空 target，而是明確錯誤，讓 allowlist 與 write gate 的意圖分離清楚。
5. `JiraClient::request` 接受 `method` 與 `Option<Value>` body，以支援 `jira_create_issue` 等寫入 stub。

## 後果

- 正向：讀工具預設可用，寫工具需顯式開啟；未知工具有明確錯誤。
- 風險：工具名稱前綴分類是保守啟發式，未來需隨 Jira API 擴充調整。
- 不含：審計日誌、回滾機制、真正的 `create_issue` payload 驗證（僅 stub）。

## 參考

- Issue #11
- PRD #1 / F4
- PR #10 加圖 Low 待辦：未知工具不應回空 target
