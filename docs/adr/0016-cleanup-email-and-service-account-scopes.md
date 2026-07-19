# ADR 0016：移除死參數 `email` 並補齊 Service Account 權限說明（Issue #40）

## 狀態
Accepted

## 背景
- Issue #33 已將 Atlassian 認證從 Basic Auth 切換為 Bearer token；`email` 欄位不再被 `JiraClient` / `ConfluenceClient` 使用。
- 目前 `AtlassianConfig.email` 只在 `config.rs` 測試的 redaction 檢查中出現，無任何實際 API 呼叫路徑（`upstream.rs` 與 `confluence.rs` 皆使用 `Authorization: Bearer <token>`）。
- README 缺少統一的「Service Account 權限設定」章節；相關 scope 散落在 `config.example.toml` 註解中，不易查閱。
- Bitbucket 擴充後（#38 / #39）也需要記錄 token 所需的 workspace/repo 權限。

## 決策

1. 移除 `AtlassianConfig.email` 死參數：
   - 從 `src/config.rs` 的 `AtlassianConfig` struct 移除 `email` 欄位。
   - 移除 `config.example.toml` 中 `[atlassian]` 的 `email` 註解。
   - 移除 `src/config.rs` 測試中對 `email` redaction 的檢查；保留 token redaction 測試。
2. README 新增「Service Account 權限設定」章節：
   - 說明 atlapool 使用 `Authorization: Bearer <token>`，不需要 `email`。
   - Atlassian Cloud：列出 Jira / Confluence 所需 OAuth scope。
   - Bitbucket：列出 App password / OAuth consumer 所需權限（`repository:read`、`pullrequest:read` 等）。
3. `config.example.toml` 的註解保留簡短版本，詳細說明移至 README。
4. 額外掃描 `src/` 尋找「定義了但無實際呼叫路徑」的參數：
   - 經審查，本次僅發現 `email` 一處死參數。

## 後果
- 正向：配置更簡潔，文件更集中，降低新用戶誤以為需要 `email` 的困惑。
- 風險：若外部使用者仍在 `config.toml` 中使用 `email`，TOML 解析將產生 unknown field 警告（預設忽略）；不影響運行。
- 不含：改變任何現有 API 行為或認證流程。

## 參考
- Issue #40
- ADR 0014（Bearer token + Confluence V2）
- `src/config.rs`、`config.example.toml`、`README.md`
