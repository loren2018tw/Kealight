# 編輯往返保留搜尋語境

Status: backlog
Type: task

## 背景

2026-09-12 修正「搜尋後編輯錯項目」時定案：本次不讓 `q`（與 `page`）跟著編輯往返（見 Q4）。本 ticket 追蹤後續。

## 現況

- 編輯 URL `/subnet/{idx}/res/{ridx}/edit` 不攜帶 `q`/`page`。
- 表單送出後 `submit_reservation`（src/web.rs）固定 redirect 回 `/subnet/{idx}`，搜尋後編輯完會掉回未過濾清單。

## 目標

讓使用者從即時篩選結果按下編輯、存檔後，回到原本的搜尋結果清單（保留 `q` 與 `page`）。需同步處理編輯網址、表單隱藏欄位與 redirect 三處。