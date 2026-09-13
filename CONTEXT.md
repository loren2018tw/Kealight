# Kealight — Kea DHCP Reservation Editor

Rust 撰寫的前後端一體工具，編輯 Kea DHCP4 設定檔中的主機保留（reservation）。v1 聚焦既有 subnet 內單筆 reservation 的新增、修改、刪除與查詢，另提供 memfile 租用（lease）的唯讀檢視，以及 kea 設定檔（可能被外部系統編輯）的外部修改偵測。

## Language

**Reservation（主機保留）**:
一筆靜態 DHCP 綁定：以 hw-address 為身分，對應一個 ip-address，可附 hostname，隸屬於某個 subnet。
身分鍵分兩層：**邏輯身分鍵**為 hw-address（唯一性語意所在）；**操作錨**（編輯／刪除時選定哪一筆的方式）由實作決定，v1 使用未過濾清單的陣列位置。
_Avoid_: 靜態 DHCP 項目、host entry、租用（lease 是不同概念，見下）

**Lease（租用）**:
Kea 執行期的暫態 DHCP 記錄，代表一個目前（或最近）被實際發放的綁定；以 memfile 的 CSV（每 server 一個檔）為儲存。以數字 state（0=已指派、1=declined、2=expired-reclaimed、3=released、4=registered）與 expire（epoch 秒）判定狀態；同一 address 可能有多列（append-only），讀取應取最後一列。
與 reservation 的關係：reservation 是設定檔中的靜態意圖，lease 是執行結果；保留位址被實際發放後會以 state=0 的 lease 出現。Kealight 對 lease 一律唯讀，不寫回。
_Avoid_: 動態保留、binding

**Subnet（子網）**:
承載 reservations 的容器（如 10.1.0.0/16），同時界定 reservation 的 IP 合法範圍。v1 中視為唯讀，僅作為編輯目標的選擇單位。
_Avoid_: 網段（僅口語）

**hw-address**:
reservation 的身分鍵（MAC）。唯一性以 subnet 為界：單筆新增／編輯時的衝突檢查在 subnet 內比對，同一 subnet 內不得重複；快貼（paste）語境則以 hw-address 為覆寫（upsert）鍵，相符即更新而非衝突。
_Avoid_: MAC、硬體位址（僅口語）

**hostname**:
reservation 的選用名稱欄位，不具唯一性。

**Dynamic Pool（動態發放池）**:
subnet 內由 Kea 自動發放的 IP 範圍（如 10.1.11.1 - 10.1.11.250）。reservation 的 IP 不得落入此範圍。

**Conflict（衝突）**:
違反唯一性或範圍規則的狀況：hw-address 重複、IP 被其他 reservation 佔用、IP 落入 dynamic pool、IP 超出 subnet 範圍。單筆新增／編輯時一律拒絕寫入；重複的 hostname 僅警告不擋。快貼（paste）語境下 hw-address 重複不構成 Conflict，而是以匯入值覆寫既有筆。
_Avoid_: 錯誤、重複（重複只是衝突的一種）

**Paste（快貼）**:
批次新增／更新 reservation 的操作：把試算表（ODS/Excel）複製的多列文字貼入多行輸入框，一次性套用至目前選定的 subnet。每列一筆、欄位以 Tab（該列無 Tab 時退回逗號）分隔，固定順序為 hw-address、ip-address、選用 hostname。以 hw-address 為覆寫鍵：與目前 subnet 內既有筆相符則以匯入的 ip-address／hostname 覆寫，否則新增。整批以單一次寫回落地（單一備份）；被跳過的列（首欄非 MAC 格式、缺或無效的 hw-address／ip-address、IP 衝突）逐筆附原因列入結果。
_Avoid_: 匯入、批量貼上（僅口語）

**Credential（憑證）**:
用於驗證操作者身分的共享密碼，設定於 `kealight.toml` 的 `password` 欄位。不與特定用戶綁定，為全站共用。密碼為空字串或缺省時不啟用認證。
_Avoid_: 密碼、auth（僅口語）
