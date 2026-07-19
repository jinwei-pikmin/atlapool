# ADR 0018：Jira issue_key 路徑穿越修正（Issue #48）

## 狀態

Accepted

## 背景

`jira_get_issue` 與 `jira_add_comment` 在 `resolve_target()` 中只對 `issue_key` 做 `split_once('-')`，把 `-` 之前的字串當作 `project` 用於 allowlist 檢查，但接著把整個 `issue_key` 直接拼入 REST 路徑：

```
/rest/api/3/issue/{issue_key}
```

`reqwest` 底層的 `url` crate 會依 WHATWG URL 規範對 `..` 做正規化。因此惡意 `issue_key` 如 `PROJ-1/../../user/search` 雖然通過 project 檢查，實際卻被送往 `/rest/api/3/user/search`，造成路徑穿越（OWASP A01）。

`jira_add_comment` 為 POST，影響更重：攻擊者可用任意 ADF JSON body 呼叫其他 Jira REST 端點。

Bitbucket 同類問題已在 PR #42 以白名單修復，Jira issue_key 卻漏掉相同防護。

## 決策

1. 新增 `valid_issue_key(s: &str) -> Result<(), String>`：
   - 必須包含至少一個 `-`。
   - `-` 之前的 project part 只能由 ASCII 英數與底線 `_` 組成。
   - `-` 之後的 number part 必須為純數字。
   - 同時適用於 `jira_get_issue` 與 `jira_add_comment`。
2. `resolve_target()` 在解析 `issue_key` 後先執行 `valid_issue_key(issue_key)?`，失敗回傳 400，不組路徑。
3. `project` 仍由 `split_once('-')` 取得，用於 allowlist 檢查；由於 `-` 之前的字串已受限於 ASCII 英數與 `_`，不可能再包含 `/`、`..` 等穿越字元。
4. 單元測試新增：
   - `PROJ-1/../../user/search` → 400，不送上遊。
   - 缺少 `-` 與 number part 非數字 → 400。

## 後果

- 攻擊者無法再用 `issue_key` 逃出 `/rest/api/3/issue/` 命名空間。
- project 解析與路徑穿越由同一份正規化邏輯同時堵住，不會互相脫勾。
- 與 `repo_slug`、`page_id` 等已實作的驗證風格一致。
