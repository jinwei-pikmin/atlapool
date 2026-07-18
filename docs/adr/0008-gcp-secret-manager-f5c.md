# ADR 0008：Secret 解析器 — GCP Secret Manager 後端（F5-c）

## 狀態
Accepted

## 背景
F5-a `env:` 與 F5-b `aws:secretsmanager:` 已完成。三種 secret 後端的最後一塊是 `gcp:secretmanager:`，讓 atlapool 在 GCP 部署時能從 Secret Manager 啟動時載入 credential，而不把密鑰寫入 config。

## 決策

1. 擴充 `src/secrets.rs`：
   - 沿用 F5-b 建立的 `SecretBackend` async trait。
   - 新增 `GcpSecretsManager` 實作，使用 `google-cloud-secretmanager-v1` 標準客戶端，走 GCP Application Default Credentials（ADC），不進 config。
   - `gcp:secretmanager:` 後的資源字串支援兩種形式：
     - `<project>/<secret>` → 自動組成 `projects/{project}/secrets/{secret}/versions/latest`
     - 完整 resource name `projects/.../secrets/.../versions/...` → 直接使用
   - 解析失敗（secret 不存在、存取權不足、ADC 無法初始化）視為 `SecretError`，啟動即退出（fail-closed）。
2. 直接採用 **惰性初始化**：
   - 新增 `LazyGcpBackend`，內部用 `std::sync::OnceCell` 或 `tokio::sync::OnceCell` 包 `GcpSecretsManager`。
   - `Config::load` 只在遇到 `gcp:secretmanager:` 前綴時才觸發 client 建立，避免 `env:`-only 或 `aws:`-only 部署無故嘗試載入 GCP ADC。
3. `resolve` 維持 async，在 `gcp:secretmanager:` 分支中呼叫 `SecretBackend::get_secret`。
4. GCP credential 不進 config，完全交給 ADC（`GOOGLE_APPLICATION_CREDENTIALS`、workload identity、metadata server 等）。
5. 單元測試使用 mock backend（實作 `SecretBackend` trait）驗證成功解析與失敗路徑，不依賴真實 GCP。

## 後果

- 正向：`env:`、`aws:`、`gcp:` 三種 secret 後端到齊；GCP 部署可透過 IAM / workload identity 取 credential；credential 不落 config；啟動失敗即退出。
- 風險：`google-cloud-secretmanager-v1` 引入 `gax`、`gaxi` 與 rustls provider，編譯時間與 binary 體積增加。替代方案如 `google-secretmanager1`（REST）需要自行處理 OAuth2 token 刷新與服務帳號金鑰，違反 ADC 原則且把憑證管理複雜化，故不採用。
- 不含：GCP 部署腳本、Firestore 隔離設定；這些屬於阿格里帕後續工作，並須在 ADR 列出所需 GCP 資源與 env 鍵。

## GCP 資源與環境變數

- `GOOGLE_APPLICATION_CREDENTIALS`（可選）：ADC 服務帳號金鑰路徑或 workload identity 憑證。
- `GOOGLE_CLOUD_PROJECT`（本 feature 不需要，因 resource name 已含 project）：預留，供日後其他 GCP 整合使用。

## 參考

- Issue #15
- PRD #1 / F5
- ADR 0002（env: 後端）
- ADR 0007（aws:secretsmanager: 後端，含惰性初始化教訓）
