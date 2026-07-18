# ADR 0010：/health 端點（F8）

## 狀態
Accepted

## 背景
atlapool 已具備 `/stats` 端點，但容器編排與負載平衡器需要一個純粹的存活檢查（liveness）端點，不應因外部依賴（Jira、AWS/GCP secret manager）抖動而誤判服務不健康。

## 決策

1. 新增 `GET /health`：
   - 回應 HTTP 200 + `{"status":"ok"}`。
   - 不檢查 `JiraClient`、`Config` 中的 secret backend、audit log 或任何外部網路狀態。
   - 只驗證 atlapool 程序本身正在執行並能回應 HTTP。
2. 與 `/stats` 分開：
   - `/stats` 可暴露運行資訊（如 uptime、port）。
   - `/health` 保持最小回應，避免健康檢查洩露過多資訊或引入額外延遲。
3. 單元測試驗證狀態碼與回應 body，確保行為穩定。

## 後果

- 正向：部署於 AWS Fargate / k3s 時，負載平衡器與 orchestrator 有可靠存活探針；外部服務抖動不會導致服務被誤殺。
- 風險：`/health` 僅為 liveness，不反映 readiness（例如 Jira 連線中斷時仍回 ok）。若未來需要 readiness probe，應新增 `/readyz` 而非擴充 `/health`，以維持語義清晰。
- 不含：readiness 檢查、外部依賴健康彙總、認證機制。

## 參考

- Issue #19
- PRD #1 / F8
