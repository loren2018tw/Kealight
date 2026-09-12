# ADR 0002: reload 直接連 kea 設定檔宣告的 control-socket 發 config-reload

「套用設定」的 kea hot reload，不走 `systemctl reload` / SIGHUP，也不依賴額外的 kea-ctrl-agent HTTP daemon；工具 parse kea 設定檔後，直接連線其 `control-socket` 宣告的端點（unix socket 或 TCP），發送 `{"command": "config-reload"}`，以 JSON 回應確認成敗。

理由：工具本來就要 parse 設定檔，socket 設定就在裡面，零額外 daemon、不需 `kea_api` 設定欄位；命令通道有回應，可向使用者呈現成功/失敗，而 service reload 是 fire-and-forget 無回應。代價是需與 kea 同主機且有 socket 權限——install.sh 以 systemd 服務同機執行，此前提成立。連不上或未宣告時，「套用設定」停用並提示；reload 失敗時檔案已寫入、僅報錯不回滾（安全由時間戳備份兜底，見 ADR 0001）。
