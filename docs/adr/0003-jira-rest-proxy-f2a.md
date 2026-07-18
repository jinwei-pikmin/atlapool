# ADR 0003：上游 REST 代理骨架（F2-a：Jira 單一 GET 端點）

## 狀態
Accepted

## 背景
atlapool 需要代理 client 請求到 Jira Cloud REST API，並在 server 端注入 Atlassian credential，使 client 端不直接持有 token。F2-a 先做一條最小路徑，證明「client 不帶 credential、server 注入 token」的鏈路可行。

## 決策

1. 新增 `src/upstream.rs`，封裝 `JiraClient`：
   - 從 `AtlassianConfig` 取得 `base_url` 與 `SecretString` token。
   - 提供 `myself_request()` 建立 `GET /rest/api/3/myself` 請求。
2. `Authorization` header 由 server 端注入：`Bearer <token>`（F2-a 煙霧測試方案；實際 Atlassian Basic/OAuth 認證於 F2-b 根據 PRD/部署測試調整）。
3. 完全丟棄呼叫端傳入的 `Authorization`、`Cookie` 等敏感 header，不透傳；F2-a 先以空 header 重新組建。
4. 僅供內部測試呼叫，不對外暴露 endpoint（F1 後續處理）。
5. 新增 `reqwest` 依賴，`rustls-tls` 避免系統 OpenSSL，維持容器可移植。
6. 本階段未新增 GCP 資源；既有 `Dockerfile`/CI 仍可部署。

## 後果

- 正向：client credential 不落地、敏感 header 重建、代理鏈路可單元測試。
- 風險：Bearer 可能非 Jira API token 最終認證方式，F2-b 需驗證並調整。
- 不含：Confluence、per-agent/allowlist、MCP endpoint。

## 參考

- Issue #5
- PRD #1 / F2
- ghpool `src/main.rs` proxy 與 `reqwest` 用法
