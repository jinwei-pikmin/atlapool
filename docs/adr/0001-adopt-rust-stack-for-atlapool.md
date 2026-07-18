# ADR 0001：atlapool v1 採用 Rust 技術棧

## 狀態
Accepted（凱撒裁決 GO）

## 背景
atlapool 為 Atlassian（Jira / Confluence）credential-swapping proxy，與 ghpool 場景一致：代理 REST/MCP 流量、server-side credential 管理、per-agent allowlist、fail-closed audit。

## 決策
沿用 ghpool 的 Rust 技術棧，理由如下：

1. 場景與 ghpool 高度相似，可借鏡其模組劃分與依賴選型。
2. Rust 編譯為單一靜態 binary，適合容器與 Cloud Run / GKE 部署。
3. axum + tokio + tower-http 生態成熟，可同時服務 MCP Streamable-HTTP 與 REST proxy。
4. 型別安全與所有權模型降低 credential 在 process memory 外洩風險，符合 G1 credential 不落地訴求。

## 後果

- 正向：與 ghpool 共享工具鏈、CI 樣板、部署模式；單一 binary 啟動快。
- 風險：GCP Secret Manager 解析器需於 F5 重新實作（ghpool 尚未支援）；Rust 學習曲線對後續貢獻者可能較高。
- 模組：初期為單一 bin；未來視需要再拆 workspace。

## 參考

- Issue #1（PRD）
- Issue #2（scaffold）
- ghpool: https://github.com/openabdev/ghpool
