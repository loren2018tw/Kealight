# ADR 0004: Lease 檔路徑由 kea 設定檔 lease-database 讀取

「作用中租用」分頁需要 memfile CSV 的路徑。我們決定**不**在 kealight.toml 新增欄位，而是從 kea 設定檔的 `lease-database` 讀取：`type` 缺失或非 memfile 時顯示空狀態；`type = memfile` 無 `name` 時用預設 `/var/lib/kea/kea-leases4.csv`；`name` 含 `/` 照字面使用，裸檔名（Kea ≥2.7.9 的常態）與 `/var/lib/kea/` 組合。

理由：kea 設定檔本身就能宣告這個路徑，工具既然要 parse 它，重複在 kealight.toml 宣告只是創造第二個可能漂移的來源——原本規劃的欄位在研究後取消。代價是需理解 kea 的 `name` 語意（受限於編譯期 data 目錄、常為裸檔名），且若 kea 日後改預設路徑，本工具需跟上。