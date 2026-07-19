# ADR 0014：Service Account scoped token 使用 Bearer auth 與 Confluence V2 API（Issue #33）

## 狀態
Accepted

## 背景
PR #31 引入 Basic Auth（`Authorization: Basic <base64(email:token)>`）與 Confluence V1 Content API（`/wiki/rest/api/content/{id}?expand=body.storage`）。經至尊以實際 Service Account scoped token curl 驗證後，發現以下假設錯誤：
- Service Account scoped token 必須使用 **Bearer Auth**：`Authorization: Bearer <token>`。
- Confluence Cloud 對 scoped token 應使用 **V2 API**：`/wiki/api/v2/pages/{page_id}?body-format=view`。

## 決策

1. `JiraClient` / `ConfluenceClient` 的 `request` 改回 **Bearer Auth**：
   - 從空 header 開始，僅注入 `Authorization: Bearer <token>`。
   - 不再使用 `email` 進行 Basic Auth。
2. `AtlassianConfig.email` 改為 **可選**（保留欄位供未來 classic token / Basic Auth 場景，但不 required）。
3. `ConfluenceClient::get_page_request` 與 `mcp.rs resolve_target` 改回 V2 路徑：
   - `/wiki/api/v2/pages/{page_id}?body-format=view`
4. 生產環境仍透過 `cloud_id` 走 `api.atlassian.com` 閘道；`base_url` 維持為測試/人類可讀連結的 fallback。
5. README / `config.example.toml` 同步更新：
   - 說明 Service Account scoped token 使用 Bearer
   - Confluence 使用 V2 API
   - `email` 註解改為可選
6. 單元與 E2E 測試更新：
   - `Authorization` header 預期從 `Basic ...` 改回 `Bearer test-token`
   - Confluence mock 路由改回 `/wiki/api/v2/pages/{id}`

## 後果
- 正向：與 Service Account scoped token 的實際 Atlassian Cloud REST API 行為一致，真實端到端可打通。
- 風險：若未來支援 classic token Basic Auth，需重新引入 email 與 Basic Auth 分支；目前先以 Service Account scoped token 為主。
- 不含：OAuth2/OIDC 完整 flow、token refresh、rate limit / retry。

## 參考
- Issue #33
- Issue #25 / PR #31
- Issue #29
