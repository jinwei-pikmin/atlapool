# ADR 0004：Per-agent Identity + Allowlist（F3）

## 狀態
Accepted

## 背景
atlapool 必須支援多個 agent，每個 agent 持有自己的 key，並對 tool、project、space 實施 default-deny allowlist。client 不直接持有 Atlassian credential，而是透過 agent key 換取對應的權限範圍。

## 決策

1. `AgentConfig` 置於 `src/agents.rs`，欄位：
   - `id`：agent 識別名稱。
   - `keys`：多把 key（`Vec<SecretString>`），支援未來 F6 熱輪替。
   - `tools`：允許的 tool 名稱清單。
   - `projects` / `spaces`：允許的 Jira project / Confluence space 清單，支援 `*` 萬用字元。
2. 頂層 `[[agents]]` 區塊由 `Config` 解析，key 若為 `env:` reference 則於 `Config::load` 解析成明文並以 `SecretString` 儲存，`Debug`/`Display` 一律遮罩。
3. `src/agents.rs` 提供：
   - `find_agent(agents, key)`：以 client 提供的 key 找到對應 agent，找不到即未授權。
   - `AgentConfig::authorize(tool, project, space)`：
     - tool 不在 `tools` → deny。
     - `project` 與 `space` 皆無法解析（皆為 `None`/empty）→ deny（deny-if-unresolvable）。
     - project 或 space 任一解析成功且命中對應 allowlist → allow。
     - 兩者皆解析但皆未命中 → deny（default-deny）。
4. 萬用字元語義：`*` 匹配任意（含空）字元序列，可跨 `/`；`?` 不支援。
5. 本階段僅純邏輯與單元測試，不接 HTTP 層（F1 後續）。

## 後果

- 正向：AC2/AC4 邏輯層基礎完成；keys 與 token 同級遮罩；default-deny 可測。
- 風險：萬用字元 `*` 可跨 `/`，與嚴格 path-glob 不同，需在 ADR/文件說明。
- 不含：HTTP endpoint 掛載、key 熱輪替同時生效測試（F6）、Confluence 專屬邏輯（可復用 `spaces` allowlist）。

## 參考

- Issue #7
- PRD #1 / F3
- ghpool `McpAgentConfig` allowlist 概念
