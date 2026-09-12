# Kealight — Kea DHCP Reservation Editor

Rust 撰寫的前後端一體工具，編輯 Kea DHCP4 設定檔中的主機保留（reservation）。v1 聚焦既有 subnet 內單筆 reservation 的新增、修改、刪除與查詢。

## Language

**Reservation（主機保留）**:
一筆靜態 DHCP 綁定：以 hw-address 為身分，對應一個 ip-address，可附 hostname，隸屬於某個 subnet。
身分鍵分兩層：**邏輯身分鍵**為 hw-address（唯一性語意所在）；**操作錨**（編輯／刪除時選定哪一筆的方式）由實作決定，v1 使用未過濾清單的陣列位置。
_Avoid_: 靜態 DHCP 項目、host entry、lease

**Subnet（子網）**:
承載 reservations 的容器（如 10.1.0.0/16），同時界定 reservation 的 IP 合法範圍。v1 中視為唯讀，僅作為編輯目標的選擇單位。
_Avoid_: 網段（僅口語）

**hw-address**:
reservation 的身分鍵（MAC）。同一設定檔內不得重複。
_Avoid_: MAC、硬體位址（僅口語）

**hostname**:
reservation 的選用名稱欄位，不具唯一性。

**Dynamic Pool（動態發放池）**:
subnet 內由 Kea 自動發放的 IP 範圍（如 10.1.11.1 - 10.1.11.250）。reservation 的 IP 不得落入此範圍。

**Conflict（衝突）**:
違反唯一性或範圍規則的狀況：hw-address 重複、IP 被其他 reservation 佔用、IP 落入 dynamic pool、IP 超出 subnet 範圍。v1 一律拒絕寫入。
_Avoid_: 錯誤、重複（重複只是衝突的一種）
