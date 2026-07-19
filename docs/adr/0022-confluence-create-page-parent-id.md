# ADR 0022：`confluence_create_page` 支援 `parent_id`（Issue #37）

## 狀態

Accepted

## 背景

E2E 測試發現 `confluence_create_page` 無法指定新頁面的父頁面。Confluence Cloud REST API v2 在建立頁面時可於 body 內加入 `parentId` 欄位來指定父頁面；目前 atlapool 的 body 只包含 `spaceId`、`status`、`title`、`body`，缺少 `parentId`。

## 決策

1. `confluence_create_page` 增加一個 **可選** 參數 `parent_id`。
2. `parent_id` 若提供，必須是純數字字串，與 `space_id` / `page_id` 的驗證規則一致。
3. 將 `parentId` 以字串形式放入 upstream JSON body，與 `spaceId` 同層。
4. `parent_id` 不影響 allowlist 維度；allowlist 仍只檢查 `space`。
5. 更新 `README.md` 的工具表格與與範例，說明 `parent_id` 為選填。

## 後果

- 呼叫者可建立有正確層級關係的 Confluence 頁面。
- 保持向後相容：未提供 `parent_id` 時行為不變。
- 不需要改動 `ConfluenceClient` 本身；只需調整 `mcp.rs` 的 `ToolTarget` 建構。
