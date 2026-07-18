# ADR 0009：Fail-closed Audit Log（F7）

## 狀態
Accepted

## 背景
atlapool 已具備 MCP endpoint、per-agent allowlist 與 F4 write-gate。為滿足可稽核目標（G4），所有寫入操作必須留下不可抵賴的 JSONL 稽核紀錄，且稽核寫入失敗時操作必須被阻擋（fail-closed）。

## 決策

1. 新增 `src/audit.rs`：
   - `AuditLog` 負責以 JSONL 格式追加寫入稽核紀錄。
   - 每筆紀錄欄位：`agent_id`、`tool`、`target`（project 或 space）、`timestamp`（RFC 3339）、`result`（此處固定為 `"attempt"`，表示已放行嘗試）。
   - 紀錄在實際轉發上游**之前**寫入；只有寫入成功才繼續執行上游請求。
   - 寫入失敗（檔案無法開啟、無權限、磁碟滿等）→ `mcp_handler` 直接回傳 500，不轉發上游。
2. `Config.audit.path`：
   - 透過設定檔 `[audit]` 區塊設定，預設值為 `atlapool-audit.jsonl`（工作目錄）。
   - 若路徑無效，fail-closed 行為會在發生寫入操作時以 500 拒絕，而非默默遺失稽核。
3. `AppState` 加入 `audit: Option<AuditLog>`：
   - `main` 啟動時從 `config.audit.path` 建立 `AuditLog`；`mcp_handler` 在寫入操作前呼叫。
   - 測試可注入 `AuditLog` 或 `None` 來驗證成功與失敗路徑。
4. 只在 `classify_tool` 為 `Write` 且 `enable_writes=true` 放行後才寫 audit；`Read` 操作不寫 audit。
5. `timestamp` 使用系統時間 API（`time` crate 的 `OffsetDateTime::now_utc()`），不用任何 workflow 腳本層的 `Date.now()`。
6. 測試：
   - 正常寫入 → audit 檔案產生一筆紀錄，上游收到請求，操作成功。
   - 模擬 audit 寫入失敗（指向不存在的目錄）→ 操作回 500，上游未收到請求。
   - 讀操作 → audit 檔案無新增紀錄。

## 後果

- 正向：寫入操作都有稽核軌跡；audit I/O 失敗會直接阻擋操作，避免「先放行、後補紀錄」的安全漏洞。
- 風險：每筆寫入都有一次檔案開啟/寫入/flush，高頻寫入可能成為瓶頸；日後可改為背景 batch 或專屬 actor，但這會改變 fail-closed 語義，必須重新評估。
- 不含：audit log 輪轉、簽章、遠端匯出；這些屬於運維與後續強化。

## 參考

- Issue #17
- PRD #1 / F7、G4
- ADR 0006（write-gate F4）
