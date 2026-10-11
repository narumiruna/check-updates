# check-updates

檢查 `pyproject.toml` 中 Python dependencies 在 PyPI 上的最新穩定版本，並選擇要套用的更新。

使用 [`uv`](https://docs.astral.sh/uv/getting-started/installation/) 的 `uvx` 執行，不需要將 check-updates 安裝到專案環境。工具只修改 dependency requirements，不會安裝套件、修改 lock file，或執行 `uv lock`、`uv sync`。

## 快速開始

在包含 `pyproject.toml` 的目錄執行：

```sh
# 列出可用更新，不修改檔案。
uvx check-updates

# 選擇更新、預覽 diff，確認後寫入。
uvx check-updates -i

# 直接套用所有可用更新，不詢問確認。
uvx check-updates -u
```

更新後，請檢查變更並測試專案。若使用 uv 管理專案，可再自行執行 `uv lock` 或 `uv sync`。

### 常用選項

| 選項 | 用途 |
| --- | --- |
| `-i`, `--interactive` | 選擇更新，預覽 diff，確認後寫入 |
| `-u`, `--upgrade` | 直接寫入所有可用更新 |
| `-w`, `--workspace` | 檢查 uv workspace root 與所有納入的 members |
| `--help` | 顯示命令說明 |

`-i` 與 `-u` 不能同時使用；兩者都可以搭配 `-w`。不指定 `-i` 或 `-u` 時，只列出更新。

### 輸出範例

```text
Dependency updates

pyproject.toml:
╭─────────────────────┬────────┬────────┬──────────────────────╮
│ Current requirement │ Target │ Change │ Section              │
├─────────────────────┼────────┼────────┼──────────────────────┤
│ typer>=0.27.1       │ 0.27.3 │ patch  │ project.dependencies │
╰─────────────────────┴────────┴────────┴──────────────────────╯

1 dependency with updates in 1 file.
Change labels compare requirement versions, not installed versions or compatibility.
No files changed. Use -u to apply all updates or -i to select updates.
```

表格依專案檔案分組，顯示原始 requirement、目標版本、版本變更類型與 dependency section。長 requirement 會自動換行。終端輸出使用色彩，重新導向的輸出不含 ANSI 色碼。

`major`、`minor`、`patch` 比較的是 requirement 中的版本，不是已安裝版本。未指定版本時標示為 `unbounded`；epoch 變更與其他 PEP 440 形式使用 `other`。這些標籤不代表相容性保證，包含版本低於 1.0 的套件。

## 互動更新

```sh
uvx check-updates -i
```

工具會先讀取檔案並完成 PyPI 查詢，再顯示選單。更新項目依專案檔案分組，預設全部未選取；重複出現的 dependency 可以分別選取。

| 按鍵 | 操作 |
| --- | --- |
| 方向鍵 | 移動至更新項目或專案操作 |
| Space | 切換項目的選取狀態；在專案操作上可全選或清除該專案 |
| Enter | 預覽已選取更新；沒有選取時直接結束 |
| Escape | 取消，不寫入檔案 |
| Ctrl+C | 中止，exit code 為 `130` |

選取後會顯示 unified diff，以及 dependency 與檔案數量。`Apply these changes?` 預設為 No：按 `y` 套用、`n` 取消，或以方向鍵、Space 切換後按 Enter。只有已選取的更新會寫入。

互動模式需要終端輸入與輸出。自動化流程請使用不需互動的 `uvx check-updates` 或 `uvx check-updates -u`。

寫入前會確認所有受影響檔案仍與讀取時相同。如果檔案在預覽期間被修改、刪除或變成無法讀取，工具會停止且不寫入任何檔案。此檢查無法防止檢查完成後的並行修改。

## uv workspace

加上 `-w` 可檢查 workspace root 與所有納入的 members：

```sh
# 列出整個 workspace 的更新。
uvx check-updates -w

# 選擇並確認 workspace 更新。
uvx check-updates -i -w

# 直接套用整個 workspace 的更新。
uvx check-updates -u -w
```

可從 workspace root、member 或 member 子目錄執行。工具會從目前目錄向上尋找最近的、含有 `[tool.uv.workspace]` 的 `pyproject.toml`，並依 `members` 與 `exclude` 的 glob patterns 選取專案：

```toml
[tool.uv.workspace]
members = ["packages/*"]
exclude = ["packages/seeds"]
```

- 輸出會標明每個專案檔案，PyPI 查詢結果會在專案間共用。
- `[tool.uv.sources]` 中標記為 `workspace = true` 或指向外部 workspace 的 dependencies 會略過；不會繼續檢查外部 workspace。
- Root 的 source 定義會套用至 members，除非 member 有自己的覆寫。
- 找不到 workspace、選取的 member 缺少 `pyproject.toml`，或讀取與查詢失敗時，工具會停止並回傳 exit code `1`。

未使用 `-w` 時，只檢查目前目錄的 `pyproject.toml`。Workspace 探索遵循 [uv workspace 設定](https://docs.astral.sh/uv/concepts/projects/workspaces/#getting-started)，不會執行 uv。

## 更新範圍與限制

- 檢查 `[project].dependencies`、`[project.optional-dependencies]` 與 `[dependency-groups]`。
- 選取 PyPI 上至少有一個未被 yanked 的 distribution file 的最高穩定版本，忽略 prereleases 與 development releases。
- 更新單一 `>=`、`==`、`~=` version constraint；沒有 version constraint 時加入 `>=VERSION`。不會降低現有版本。
- 保留 extras、environment markers、註解、引號、TOML 格式與換行形式。帶有 marker 的 dependency 仍會檢查，不受目前環境影響。
- 回報並略過 URL dependencies、wildcard pins、多重 constraints、排除條件、版本上限與嚴格的 `>` 下限。Dependency-group include entries 保持不變。

工具不會解析 dependency 相依關係，也不會檢查 Python 版本相容性。安裝更新前，請自行確認相容性。

所有檔案讀取與 PyPI 查詢都會在寫入前完成，因此查詢失敗不會造成部分更新。但多檔案寫入是依序執行，並非 transaction：若寫入失敗，先前成功的更新會保留。請依輸出與 version control 檢查或還原變更。

### Exit codes

| Code | 意義 |
| --- | --- |
| `0` | 正常完成，包含沒有更新、沒有選取或取消確認 |
| `1` | 檔案、解析或網路錯誤 |
| `2` | 無效選項組合，或互動模式沒有終端輸入與輸出 |
| `130` | 由 Ctrl+C 中止 |

## 開發

在 repository 中安裝開發環境：

```sh
uv sync --frozen
```

執行檢查，不更新環境或 lock file：

```sh
uv run --frozen --no-sync pytest
uv run --frozen --no-sync ruff check .
uv run --frozen --no-sync ty check .
```
