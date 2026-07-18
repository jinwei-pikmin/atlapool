# ADR 0007：Secret 解析器 AWS Secrets Manager 後端（F5-b）

## 狀態
Accepted

## 背景
F5-a 已完成 `env:VAR_NAME` 解析器。正式部署將走 AWS Fargate，因此需要 `aws:secretsmanager:<secret-id>` 後端，讓 atlapool 在啟動時從 AWS Secrets Manager 取得 credential，而不將密鑰寫入 config。

## 決策

1. 擴充 `src/secrets.rs`：
   - 新增 `SecretBackend` async trait，提供 `get_secret(secret_id) -> Result<String, SecretError>`。
   - 新增 `AwsSecretsManager` 實作，使用 `aws-config` 標準 credential chain + `aws-sdk-secretsmanager` 呼叫 `GetSecretValue`。
   - 保留 `env:` 分支，由 `resolve` 直接讀取環境變數，無需 backend。
2. `resolve` 改為 async，並接受 `&impl SecretBackend`。
   - 前綴 `aws:secretsmanager:` 後的整段字串視為 `secret-id`（可為 secret name 或完整 ARN，內部可含 `:`）。
3. `Config::load` 與 `Config::from_toml` 改為 async，並把 backend 注入 `resolve`。
4. `main` 在啟動時建立 `AwsSecretsManager`，並在 `AWS_REGION` 未設或 SDK 初始化失敗時立即回傳錯誤，由 `main` 非零退出（fail-closed）。
5. AWS credential 不進 config，完全交給 `aws-config` 的標準機制（環境變數、IAM role、~/.aws 等）。
6. 單元測試使用 mock backend（實作 `SecretBackend` trait）驗證成功解析與失敗路徑，不依賴真實 AWS。

## 後果

- 正向：credential 不落地 config；正式 AWS 部署可直接使用 IAM role；啟動失敗即退出，避免靜默降級。
- 風險：`resolve` 變為 async，`Config::load` 與 `main` 隨之 async 化，呼叫鏈路增加一層 `await`。
- 不含：GCP Secret Manager（F5-c）、AWS 部署腳本（阿格里帕後續工作）。

## 參考

- Issue #13
- PRD #1 / F5
- ADR 0002（env: 後端）
