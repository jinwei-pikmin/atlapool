# ADR 0021：接上 `[mcp] enabled` 與 `enable_writes` 設定（Issue #26）

## 狀態

Accepted

## 背景

`[mcp]` 區塊的 `enabled` 與 `enable_writes` 欄位在 `config.example.toml` 與 `Config` 結構中已存在，但 `src/mcp.rs` 的 `mcp_handler` 完全沒有使用：

- `mcp.enabled` 對 `/mcp` 路由無效，endpoint 隨時可呼叫。
- `mcp.enable_writes` 對 write-gate 無效，只有 `agents[].enable_writes` 生效。

這造成文件與程式行為不一致，屬於技術債。

## 決策

1. `mcp.enabled` 設為 `false` 時，`/mcp` 直接回傳 `503 Service Unavailable`，訊息為 `mcp endpoint disabled`。
2. `mcp.enable_writes` 作為 write-gate 的全域預設值；`agents[].enable_writes` 若設定則覆蓋全域值。
3. 將 `AgentConfig.enable_writes` 從 `bool` 改為 `Option<bool>`，以便區分「未設定」與「明確 false」。
   - 未設定：`agent.enable_writes` 為 `None`，套用 `[mcp] enable_writes`。
   - `Some(false)`：即使 `[mcp] enable_writes = true`，該 agent 仍禁止寫入。
   - `Some(true)`：該 agent 可以寫入，即使全域為 false。
4. 預設值維持向後相容：未提供 `[mcp]` 時 `enabled = false`、`enable_writes = false`；未提供 agent `enable_writes` 時視同 `None`。

## 後果

- 管理員可一鍵關閉 `/mcp` endpoint，而不影響 `/health` 與 `/stats`。
- 可透過 `[mcp] enable_writes = true` 統一開啟所有 agent 的寫入權限，再針對特定 agent 用 `enable_writes = false` 顯式關閉。
- 需要更新 `config.example.toml` 與 README 的欄位說明，並補充單元測試。
