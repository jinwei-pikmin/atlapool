# ADR 0020：README 大改版（Issue #51 文件後續）

## 狀態

Accepted

## 背景

v0.1 功能已經齊全，但 README 仍偏「快速上手」取向，缺少與 ghpool 對標的架構與設計原則說明。Issue #51 與格拉古（使用者視角）要求文件必須誠實反映：

1. atlapool 與 ghpool 在定位上的異同（同為 MCP/REST credential proxy）。
2. 目前的憑證模型限制（長效 token 轉發 vs 短效 token 鑄造）。
3. 已實現的保證與未來排程（v0.1 vs v0.2）。

## 決策

1. 保留既有 Quick start、Configuration、Service account permissions、Agent allowlists 等實用章節，但在文件最前面加入：
   - **Design Principles**：5-6 條，包含 Current limitation 誠實聲明。
   - **Architecture**：ASCII 圖呈現 agent → atlapool → upstream 與 secrets/audit 的關係。
   - **Status**：v0.1 已完成 / v0.2 規劃中 的分期標示。
   - **How clients use it**：提供 MCP client（如 Claude Desktop）的 JSON 設定範例。
2. Current limitation 明確寫出：atlapool 目前把同一組長效 Service Account token 原樣轉發給 upstream，不具備 ghpool 的短效 repo-scoped token 鑄造能力，這是 v0.1 的已知架構限制，v0.2 評估改進。
3. 暫時不重寫「Read vs. write and audit」章節的欄位細節，等 Issue #52（audit result）merge 後再對齊 `audit.rs` 實際欄位。

## 後果

- 新讀者可在 30 秒內理解 atlapool 的價值主張與架構邊界。
- README 與 ghpool 的資訊結構對齊，但不抄襲；保留 atlapool 特有的 Atlassian 工具鏈與長效 token 限制。
- 文件變動僅影響 `README.md`，無需額外程式碼變更。
