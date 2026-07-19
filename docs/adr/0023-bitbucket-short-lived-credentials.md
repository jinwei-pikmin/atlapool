# ADR 0023：Bitbucket 短效憑證鑄造（Issue #54）

## 狀態

Accepted — 凱撒複核通過，已實作

## 背景

v0.1 的 Bitbucket 整合將同一組長效 Service Account token（App password 或 OAuth access token）原樣轉發給所有請求。對照 ghpool 的 GitHub App installation token 模式（1 小時 TTL、repo-scoped、自動輪替），atlapool 缺少等價的短效憑證機制。Issue #54 要求研究並選定可行方案。

## 調查結果

### Bitbucket Cloud 可用的憑證機制

1. **OAuth 2.0 client credentials grant**
   - 端點：`POST https://bitbucket.org/site/oauth2/access_token`
   - 認證：HTTP Basic `client_id:client_secret`
   - Body：`grant_type=client_credentials`
   - 回應：包含 `access_token`、`refresh_token`、`expires_in`（預設 7200 秒，即 2 小時）、`token_type`。
   - 優點：可程式化取得、有 TTL、有 refresh token、支援現有 `repository:admin` 等 OAuth scope。
   - 缺點：
     - 一次取得的 access token 作用範圍是整個 OAuth consumer 的權限（workspace 等級），**Bitbucket Cloud 不支援在單次 grant 請求用 `scope` 縮小實際權限**（提供 `scope` 只會驗證是否超出 consumer 權限，不會限制 token）。
     - 因此無法像 GitHub App 那樣「逐 repo 鑄造 scoped token」；要達到 repo-scoped 需要 Connect app / JWT exchange，複雜度大幅提高。

2. **Repository / Project / Workspace access tokens**
   - 透過 Bitbucket UI 建立，可設到期日、可綁定單一 repo/project/workspace。
   - 缺點：目前沒有公開的 REST API 讓 atlapool 在執行期「鑄造」新 token；屬於靜態長效 token，與 Issue 目標不符。

3. **Connect app JWT → OAuth access token exchange**
   - 端點同樣是 `https://bitbucket.org/site/oauth2/access_token`，但 grant type 為 `urn:bitbucket:oauth2:jwt`。
   - 可透過 `bitbucket_repository={uuid}` 限制 token 只對特定 repo 有效。
   - 缺點：需要註冊 Atlassian Connect app、產生 JWT、處理 app 安裝生命週期，遠超 v0.2 範圍。

## 決策

採用 **方案 1（OAuth 2.0 client credentials grant）** 作為 v0.2 的短效憑證機制，但明確承認：

- 取得的是 **workspace/consumer 等級 access token**，不是逐 repo scoped token。
- repo 層級的隔離仍由 atlapool 自己的 `bitbucket_repos` allowlist 與請求路徑驗證來保證。
- 真正的 repo-scoped token（Connect app JWT exchange）列為 **v0.3 或後續研究**。

## 具體設計

### 設定結構

保留現有 `[bitbucket].token`（向後相容）；新增可選 `[bitbucket.oauth]`：

```toml
[bitbucket]
workspace = "my-workspace"
# 以下二選一：
# token = "env:BITBUCKET_TOKEN"                    # 長效 token（fallback）
oauth.client_id = "env:BITBUCKET_CLIENT_ID"        # OAuth consumer key
oauth.client_secret = "env:BITBUCKET_CLIENT_SECRET"  # OAuth consumer secret
# oauth.token_url = "https://bitbucket.org/site/oauth2/access_token"  # 可選，預設值
```

- `client_id` 與 `client_secret` 使用既有的 secret reference 格式（`env:`、`aws:`、`gcp:`）。
- 若 `oauth` 存在且 `client_id`/`client_secret` 皆提供，則走 OAuth；否則仍用 `token`。

### 新增/修改檔案

- `src/bitbucket_token.rs`：新的 `BitbucketTokenCache`，處理 OAuth token 取得、快取、過期與 refresh。
- `src/bitbucket.rs`：`BitbucketClient` 改為 `TokenProvider` enum（`Static` / `OAuth`），`request` 改為 async 以等待 token。
- `src/config.rs`：新增 `BitbucketOAuthConfig` 與解析邏輯。
- `src/upstream.rs`：`UpstreamClient::build_request` 改為 async；新增 `TokenFetch` 錯誤變體。
- `src/confluence.rs`：配合 `UpstreamClient` async 調整。
- `src/mcp.rs`：`forward` 改為 `.await` build_request。
- `src/main.rs`：註冊 `bitbucket_token` 模組。
- `README.md` 與 `config.example.toml`：補上 OAuth 設定與 limitation 說明。

### Token 管理元件

- `src/bitbucket_token.rs` 的 `BitbucketTokenCache`：
  - 持有 `client_id`、`client_secret`、`token_url`、獨立的 `reqwest::Client`。
  - 快取 `access_token`、`expires_at`（UTC）、`refresh_token`。
  - 提供 `pub async fn get_token() -> Result<String, UpstreamError>`。

### 快取與並發策略

- 使用 `tokio::sync::Mutex<TokenState>` 保護快取。
- 每次 `get_token()` 鎖住狀態：
  - 若 access token 存在且尚未過期，直接回傳複本。
  - 若過期或無 token，先嘗試 `grant_type=refresh_token`（若有 refresh token），失敗則 fallback 到 `grant_type=client_credentials`。
  - 更新快取後釋放鎖。
- 此策略確保**同一時間只會有一個 token 請求**發往 Bitbucket，簡單且可預測。未來若成為瓶頸，可再改為 read/write lock 或 in-flight future 去重。

### 整合到 `BitbucketClient`

- `BitbucketClient` 內部改為 `TokenProvider` enum：
  - `Static(SecretString)`：直接 expose 既有 `[bitbucket].token`。
  - `OAuth(Arc<BitbucketTokenCache>)`：使用 OAuth 快取。
- `BitbucketClient::auth_token()` async：
  - OAuth 模式：呼叫 `cache.get_token().await`。
  - 靜態模式：直接回傳 `bitbucket.token`。
- `BitbucketClient::request` 改為 async，等待 `auth_token()` 後注入 `Authorization: Bearer <token>`。

### TTL 與過期處理

- 回應中的 `expires_in` 為秒；若缺失，保守預設 7200 秒。
- 計算有效期限時使用 grace period：`expires_in / 10`，上限 600 秒、下限 1 秒；對於 2 小時 token 約等於提前 12 分鐘 refresh，對於極短測試 token 仍至少保留 1 秒。
- 即使 token 在 grace 區間外過期，請求會收到 401；此時快取會標記為過期並於下一次請求時 refresh。

### 測試策略

- `src/bitbucket_token.rs` 單元測試：
  - 首次取得並快取，mock 只收到一次請求。
  - 過期後 refresh（`expires_in=2`，等待 3 秒）。
  - 10 個並發 `get_token()` 僅觸發一次 mock token 請求。
  - token endpoint 回傳 401 時 `get_token()` 回傳 `UpstreamError::TokenFetch`。
- `src/bitbucket.rs` 單元測試：
  - 靜態 token 注入正確。
  - OAuth 設定下，第一次 `get_repo_request()` 會觸發 token fetch 並注入 `Bearer oauth-access-1`。
  - 第二次請求使用快取，token server 請求次數仍為 1。
- 受 `UpstreamClient::build_request` async 影響，`src/mcp.rs`、`src/upstream.rs`、`src/confluence.rs` 相關測試同步調整為 `#[tokio::test]`。

## 後果

- 不再把長效 Service Account token 原樣轉發；改為 2 小時一換的 OAuth access token。
- 設定更複雜（需 consumer key/secret），但 key/secret 仍可用現有 secret backend 注入。
- **Current limitation 聲明需要更新**：短效憑證已實作，但仍是 workspace/consumer 等級，非逐 repo scoped。
- 若未來 Bitbucket Cloud 提供程式化 repository access token API，可再評估取代 OAuth。

## 參考

- Bitbucket Cloud OAuth 2.0: https://support.atlassian.com/bitbucket-cloud/docs/use-oauth-on-bitbucket-cloud/
- Bitbucket Cloud REST API 認證: https://developer.atlassian.com/cloud/bitbucket/rest/intro/
- Bitbucket Connect JWT token exchange: https://developer.atlassian.com/cloud/bitbucket/oauth-2-connect/
