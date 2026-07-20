# ADR 0024：CI 自動發布容器映像檔至 GHCR（Issue #59）

## 狀態

Accepted — 凱撒/至尊裁定後執行

## 背景

目前 `.github/workflows/ci.yml` 只跑 `cargo check` / `clippy` / `test`，沒有產出可部署的容器映像檔。過去測試環境使用 `gcloud run deploy --source .` 由 Cloud Build 現場建置，缺乏一個帶版本號、可被外部（未來 k8s/Fargate 正式環境）直接引用的 image。

## 決策

採用 **GitHub Container Registry（ghcr.io）** 作為 atlapool 的官方容器映像檔倉庫：

- atlapool 是 public OSS repo，與 GitHub Actions 原生整合。
- 使用內建 `GITHUB_TOKEN`（`packages: write` permission）即可推播，無需額外 AWS IAM。
- Image 設為 public，符合 OSS 開箱即用的精神，也方便 Fargate 直接拉取。

## 具體設計

### 觸發條件

新增獨立 workflow `.github/workflows/publish-image.yml`：

- `push` 到 `main`：產生 `latest` 與 git short sha 兩個 tag。
- `push` tag `v*`（例如 `v0.1.0`）：產生該版本號 tag。
- `workflow_dispatch`：手動輸入 `image_tag`（例如 `v0.1.0`），用於補推已經存在的 git tag 對應的 image，而不移動實際 tag。

### Tag 策略

使用 `docker/metadata-action` 自動生成：

- `type=raw,value=latest,enable={{is_default_branch}}`
- `type=sha,prefix=,suffix=,format=short`
- `type=ref,event=tag`（只在 `push` 事件時啟用）
- `type=raw,value=${{ github.event.inputs.image_tag }},enable=${{ github.event_name == 'workflow_dispatch' }}`（只在手動觸發時啟用）

### 權限與公開

- Workflow job `permissions`：`packages: write`（推送）、`contents: read`（checkout）。
- `docker/login-action` 以 `ghcr.io` 為 registry，`username: ${{ github.actor }}`，`password: ${{ secrets.GITHUB_TOKEN }}`。
- 首次推送到 GHCR 後，package 預設為 **private**。由於 `GITHUB_TOKEN` 不一定具備變更個人帳號 package visibility 的權限，因此不在 CI 中嘗試自動設公開，避免默默失敗。首次 release 需由 package 擁有者到 GitHub 的 package 設定頁手動改為 `Public`（一次性動作），之後阿格里帕以未登入的 `docker pull` 驗證 AC3。

### README

新增「Quick start with published image」段落，示範如何不 clone、直接使用 `ghcr.io/jinwei-pikmin/atlapool:v0.1.0` 啟動並掛載 `config.toml`。

## 驗收條件

- AC1：push 到 main 後，ghcr.io 出現 `latest` 與 short-sha tag。
- AC2：push git tag `v*` 後，ghcr.io 出現對應版本號 tag。
- AC3：image 為 public，未登入可 `docker pull`。
- AC4：`docker run` 該 image 可正常啟動並回應 `/health`。
- AC5：README 新增使用已發布 image 的 Quick start 範例。
