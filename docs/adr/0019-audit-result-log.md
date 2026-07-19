# ADR 0019：Audit log 補 result 記錄（Issue #51）

## 狀態

Accepted

## 背景

目前 `AuditLog` 只有 `record_attempt()`，在寫入工具發出上游呼叫前寫一筆 `result: "attempt"` JSONL。這滿足了 fail-closed gate，但無法在事後稽核時看到：

- 上游呼叫最後到底成功還是失敗；
- 失敗原因與上游狀態碼；
- 回應內容（或錯誤訊息）。

ghpool 採用「pre-flight attempt + post-flight result」兩筆 fsync'd JSONL 的模式，我們比照辦理。

## 決策

1. 在 `AuditLog` 新增 `record_result(record: AuditRecord)`，寫入第二筆 JSONL。
2. `AuditRecord` 欄位包含：
   - `timestamp` — RFC 3339
   - `agent_id` — 呼叫者
   - `tool` — MCP tool name
   - `target` — 受影響的 project / space / workspace / repo / issue key
   - `result` — `"attempt"` 或 `"success"` 或 `"failure"`
   - `upstream_status` — 上游 HTTP status code 或 0（未到上游）
   - `message` — 可選，錯誤訊息或上游 body 摘要
3. 在 `forward()` 中：
   - 寫入 attempt 後才發出上游呼叫（既有邏輯）。
   - 上游回應後，根據 `upstream_status.is_success()` 與 body 解析結果決定 `success`/`failure`。
   - 即使 result 記錄寫入失敗，仍把上游回應傳回呼叫端（fail-closed 只針對 attempt）。
   - result 寫入失敗不會讓呼叫端失敗，但會讓 record 遺失；這與 ghpool 一致：result logging 是盡力而非 fail-closed。
4. 成功判斷邏輯：
   - 上游狀態碼 2xx 且能成功解析或接收回應（含 204/空 body）視為 success。
   - 上游 4xx/5xx 或連線錯誤視為 failure。
   - 不額外檢查回應 JSON 內容（上游已負責語意錯誤）。
5. README 與 config.example.toml 同步更新：
   - 說明 audit 現況：pre-flight attempt + post-flight result 兩筆 JSONL。
   - 說明憑證模型現況：atlapool 將單一長效 Service Account token 原樣轉發，不是逐 repo/session 鑄造短效憑證，這是已知架構限制。
6. 測試：Jira / Confluence / Bitbucket 各至少一個寫入工具產生 attempt + result 兩筆 JSONL，並驗證 204 empty body 成功與 4xx 失敗情境。

## 後果

- 稽核記錄更完整，事後可追蹤每次寫入的最終結果。
- `forward()` 程式碼多一層 result logging，但仍保持 attempt 的 fail-closed 屬性。
- 測試需要檢查 JSONL 檔案前後兩行。
