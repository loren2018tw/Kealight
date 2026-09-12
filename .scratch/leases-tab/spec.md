# 首頁分頁與作用中租用（Lease）分頁

Status: ready-for-agent
Type: task

## 背景

讓首頁分為兩個分頁：既有的保留主機（reservation 編輯）功能，與新的「作用中租用」分頁，顯示 Kea DHCPv4 memfile 中的作用中租用。租用為唯讀資料。

## 規範

1. **URL 與分頁導覽**：現行 `/subnet/{idx}` 原封不動，新增 `/leases`；兩頁頂部共用分頁列（保留主機／作用中租用）；`/` 仍導向 `/subnet/0`。
2. **路徑來源**：由 kea 設定檔 `lease-database` 讀取（kealight.toml 不加欄位）：
   - 區段缺失或 `type != "memfile"` → 顯示「租用資料庫非 memfile，無法顯示」空狀態。
   - `type = memfile` 且無 `name` → 預設 `/var/lib/kea/kea-leases4.csv`。
   - `name` 含 `/` → 照字面當路徑。
   - `name` 為裸檔名 → 與 `/var/lib/kea/` 組合。
   - 檔案不存在 → 「租用檔不存在」空狀態（非錯誤頁）。
3. **作用中定義**：`state == 0` 且 `expire > now`（epoch 秒）。CSV append-only，同 address 多列以**最後一列**為現況（去重鍵 address）。預設只列作用中；表格上方提供狀態篩選：全部／作用中／過期／拒絕／已釋放。
4. **欄位**：address｜hwaddr（缺則 client-id 代替）｜hostname｜subnet（`subnet_id` 對照設定檔 subnet4 的 `id` 為 CIDR，對不到顯示 `?`）｜state（中文：已指派／拒絕／過期回收／已釋放／已註冊）｜到期時間（相對顯示「剩餘 X 天 X 時」）｜保留徽章（hwaddr 與 reservation 對應的列標記）。
5. **互動**：搜尋（address／hwaddr／hostname）與排序（address 數值／hwaddr／hostname）沿用 reservation 列表；每頁 50 筆分頁。
6. **更新時機**：每次請求重讀 CSV；表格上方提供手動「重新整理」按鈕。不自動輪詢。
7. **唯讀**：租用資料不寫回、不參與「套用設定」流程。