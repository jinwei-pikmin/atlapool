# ADR 0002：Secret reference 解析器（F5-a：env: 後端）

## 狀態
Accepted

## 背景
atlapool 需要從 config 解析 Service Account credential，且 credential 不可落地於設定檔或 log。PRD F5 要求支援 `env:`、`aws:secretsmanager:`、`gcp:secretmanager:` 三種 secret reference。F5-a 先做 `env:`，其餘後續 Issue 實作。

## 決策

1. 新增 `src/secrets.rs`，提供單一 `resolve(reference: &str) -> Result<String, SecretError>` 函式。
2. 目前僅接受 `env:VAR_NAME` 語法；其餘前綴回傳 `SecretError::Unsupported`，避免靜默降級。
3. `src/config.rs` 負責載入 TOML config，並在 `Config::load` 內對 `atlassian.token` 進行解析；解析失敗即回傳錯誤，由 `main` 以非零退出。
4. 解析後的明文 token 只存於 `Config` 結構體記憶體中，絕不輸出到 log 或回傳給 client。
5. 模組化設計：未來 aws/gcp 後端可在 `resolve` 內擴充分支，或引入 trait，不影響 config 呼叫端。

## 後果

- 正向：符合 credential 不落地（G1）、fail-closed（解析失敗即退出）、可擴充。
- 風險：`resolve` 目前為同步；aws/gcp 後端將來若需非同步 I/O，屆時再重構成 async trait。
- config 檔案未設定 `[atlassian]` 或 `token` 時，視為未啟用 upstream credential，伺服器仍可啟動（方便 healthz 測試）。

## 參考

- Issue #3
- PRD #1 / F5
- ghpool `src/config.rs` 的 `resolve_secret` 設計
