# Kealight

極速 kea dhcp 設定工具：以網頁介面直接編修 Kea DHCP4 設定檔中的主機保留（reservation）。

## 功能

- **主機保留管理**：新增、修改、刪除 Kea DHCP4 設定檔內 subnet 的 reservation（`hw-address`、`ip-address`、可選 `hostname`）。
- **衝突防護**：hw-address 重複、IP 被其他 reservation 佔用、IP 落入 dynamic pool 或超出 subnet 範圍時一律拒絕寫入。
- **動態 subnet 切換**：設定檔有多個 subnet 時可切換編輯目標。
- **即時搜尋與排序**：依 hostname / IP / hw-address 即時篩選，欄位可排序、分頁瀏覽。
- **作用中租用檢視**：唯讀檢視 memfile 租用（lease）檔，可按 state、subnet 篩選，並標示「保留」位址。
- **設定檔守護**：偵測設定檔被外部系統修改，若本系統尚有未套用變更，會請你選擇以哪一方為準。
- **一鍵套用**：透過 Kea 的 control-socket 重新載入設定，變更即刻生效；未宣告 control-socket 時自動停用。
- **自動備份**：每次寫回前自動備份設定檔，保留最近 N 份。
- **連線主機 MAC 提示**：與本系統同層網路操作時，表單自動提示目前連線主機的 MAC。
- **開機自啟**：安裝時自動註冊 systemd 服務。

## 系統需求

- Linux（systemd）
- Rust toolchain（含 `cargo`）
- Kea DHCP4 服務（若要使用「套用設定」功能）

## 安裝

### 1. 取得原始碼

```sh
git clone https://github.com/你的帳號/Kealight.git
cd Kealight
```

### 2. 安裝（編譯並註冊 systemd 服務）

```sh
sudo ./install.sh
```

步驟會自動完成：

1. 編譯 release 版本
2. 安裝到 `/opt/kealight`
3. 建立設定檔 `/opt/kealight/kealight.toml`（首次安裝時，從 `kealight.toml.example` 複製）
4. 註冊並啟用 systemd 服務（開機自啟）

### 3. 編輯設定檔

```sh
sudo nano /opt/kealight/kealight.toml
```

確認以下項目（尤其 `kea_config` 路徑必須正確）：

```toml
# kea 設定檔路徑（必填）
kea_config = "/etc/kea/kea-dhcp4.conf"

# HTTP 監聽位址與埠（預設 127.0.0.1:7777）
bind = "127.0.0.1"
port = 7777

# 每次寫回前自動備份，保留最近 N 份
backup_keep = 10
```

> 若只有本機要使用，`bind` 維持 `127.0.0.1` 即可；若要給區網內其他機器操作，請改為 `0.0.0.0` 並注意安全。

### 4. 啟動服務

```sh
sudo systemctl start kealight.service
sudo systemctl status kealight.service   # 檢查是否正常運作
```

### 5. 開啟網頁

瀏覽器開啟 `http://<伺服器IP>:7777`（本機操作則為 `http://127.0.0.1:7777`）。

## 更新

若有新的版本，在原始碼目錄執行：

```sh
./upgrade.sh
```

它會 `git pull` 拉取最新原始碼、重新編譯、停用服務、更新 `/opt/kealight/kealight`，再重新啟動服務。設定檔與備份不受影響。

## 移除

```sh
sudo ./install.sh uninstall
```

停止並移除 systemd 服務，刪除 `/opt/kealight`。

## 組態範例

完整可沿用複製的範例位於 `kealight.toml.example`；把伺服器上的實際路徑填進去後存檔即可。

## 授權

本專案原始碼授權方式請見個別相依套件與專案設定。