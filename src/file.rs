use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use ipnet::Ipv4Net;
use serde_json::{Map, Value};

use crate::domain::{IpRange, Reservation, Subnet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSocket {
    pub socket_name: String,
    pub socket_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub mtime_ns: u64,
    pub size: u64,
}

/// 讀取檔案的 stat（mtime 奈秒＋大小）。檔案不存在（或無權限）回 None。
pub fn file_stat(path: &Path) -> Result<Option<FileStat>> {
    let Ok(m) = std::fs::metadata(path) else {
        return Ok(None);
    };
    let Ok(mtime) = m.modified() else {
        return Ok(Some(FileStat { mtime_ns: 0, size: m.len() }));
    };
    let ns = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("讀取修改時間失敗: {e}"))?
        .as_nanos();
    Ok(Some(FileStat {
        mtime_ns: u64::try_from(ns).unwrap_or(0),
        size: m.len(),
    }))
}

/// memfile 租用資料庫的解析結果（ADR 0004）。
#[derive(Debug, Clone)]
pub enum LeaseDb {
    /// type = memfile，租用檔路徑已解析
    Memfile(PathBuf),
    /// 無法由 kea 設定檔得知租用檔位置（附說明文字）
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct SubnetSummary {
    pub index: usize,
    pub id: Option<u32>,
    pub cidr: String,
    pub reservation_count: usize,
}

pub struct KeaFile {
    pub root: Value,
}

impl KeaFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("讀取設定檔失敗: {}", path.display()))?;
        let root: Value = serde_json::from_str(&text).context("設定檔不是合法 JSON")?;
        if root.get("Dhcp4").is_none() {
            bail!("設定檔缺少 Dhcp4 區塊");
        }
        Ok(KeaFile { root })
    }

    pub fn subnet_count(&self) -> usize {
        self.subnet4().map_or(0, |a| a.len())
    }

    /// 每個 subnet 的摘要（索引、id、CIDR、reservation 筆數），供選擇器與租用過濾使用。
    pub fn subnet_list(&self) -> Vec<SubnetSummary> {
        self.subnet4()
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let cidr = v
                            .get("subnet")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        let id = v
                            .get("id")
                            .and_then(Value::as_i64)
                            .and_then(|n| u32::try_from(n).ok());
                        let n = v
                            .get("reservations")
                            .and_then(Value::as_array)
                            .map_or(0, |a| a.len());
                        SubnetSummary {
                            index: i,
                            id,
                            cidr,
                            reservation_count: n,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 全部 subnet 的 reservation hw-address（小寫冒號格式），供租用列標記「保留」徽章。
    pub fn reservation_hwaddrs(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(arr) = self.subnet4() else {
            return out;
        };
        for s in arr.iter() {
            let Some(res) = s.get("reservations").and_then(Value::as_array) else {
                continue;
            };
            for r in res.iter() {
                let Some(hw) = r.get("hw-address").and_then(Value::as_str) else {
                    continue;
                };
                out.push(hw.to_string());
            }
        }
        out
    }

    pub fn subnet(&self, index: usize) -> Result<Subnet> {
        let arr = self.subnet4().ok_or_else(|| anyhow!("設定檔缺少 subnet4"))?;
        let v = arr.get(index).ok_or_else(|| anyhow!("subnet 索引超出範圍: {index}"))?;
        parse_subnet(v)
    }

    pub fn add_reservation(&mut self, subnet_index: usize, r: &Reservation) -> Result<()> {
        let res = self.reservations_mut(subnet_index)?;
        res.push(reservation_to_value(r));
        Ok(())
    }

    pub fn update_reservation(
        &mut self,
        subnet_index: usize,
        res_index: usize,
        r: &Reservation,
    ) -> Result<()> {
        let res = self.reservations_mut(subnet_index)?;
        let slot = res
            .get_mut(res_index)
            .ok_or_else(|| anyhow!("reservation 索引超出範圍: {res_index}"))?;
        apply_reservation_to_value(slot, r);
        Ok(())
    }

    pub fn delete_reservation(&mut self, subnet_index: usize, res_index: usize) -> Result<()> {
        let res = self.reservations_mut(subnet_index)?;
        if res_index >= res.len() {
            bail!("reservation 索引超出範圍: {res_index}");
        }
        res.remove(res_index);
        Ok(())
    }

    pub fn control_socket(&self) -> Option<ControlSocket> {
        let cs = self.dhcp4()?.get("control-socket")?;
        Some(ControlSocket {
            socket_name: cs.get("socket-name")?.as_str()?.to_string(),
            socket_type: cs.get("socket-type")?.as_str()?.to_string(),
        })
    }

    /// 解析租用資料庫路徑（ADR 0004）：
    /// 區段缺失或 type 非 memfile → Unavailable；memfile 無 name → 內建預設；
    /// name 含 '/' → 照字面；裸檔名 → 與預設目錄組合。
    pub fn lease_db(&self) -> LeaseDb {
        let default_dir = Path::new("/var/lib/kea");
        let Some(dhcp4) = self.dhcp4() else {
            return LeaseDb::Unavailable("設定檔缺少 Dhcp4 區段，無法顯示租用".into());
        };
        let Some(db) = dhcp4.get("lease-database") else {
            return LeaseDb::Unavailable("設定檔缺少 lease-database 區段，無法顯示租用".into());
        };
        let Some(ty) = db.get("type").and_then(Value::as_str) else {
            return LeaseDb::Unavailable("設定檔 lease-database 缺少 type，無法顯示租用".into());
        };
        if *ty != *"memfile" {
            return LeaseDb::Unavailable(format!("租用資料庫型別「{ty}」非 memfile，無法顯示租用"));
        }
        let Some(name) = db.get("name").and_then(Value::as_str) else {
            return LeaseDb::Memfile(default_dir.join("kea-leases4.csv"));
        };
        if name.contains('/') {
            LeaseDb::Memfile(PathBuf::from(name.to_string()))
        } else {
            LeaseDb::Memfile(default_dir.join(name.to_string()))
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let out = serialize_pretty3(&self.root);
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let tmp = dir.join(format!(".{}.tmp", path.file_name().unwrap_or_default().to_string_lossy()));
        std::fs::write(&tmp, out).with_context(|| format!("寫入暫存檔失敗: {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("替換設定檔失敗: {}", path.display()))?;
        Ok(())
    }

    fn dhcp4(&self) -> Option<&Value> {
        self.root.get("Dhcp4")
    }

    fn dhcp4_mut(&mut self) -> Option<&mut Value> {
        self.root.get_mut("Dhcp4")
    }

    fn subnet4(&self) -> Option<&Vec<Value>> {
        self.dhcp4()?.get("subnet4")?.as_array()
    }

    fn reservations_mut(&mut self, subnet_index: usize) -> Result<&mut Vec<Value>> {
        let arr = self
            .dhcp4_mut()
            .and_then(|d| d.get_mut("subnet4"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("設定檔缺少 subnet4"))?;
        let subnet = arr
            .get_mut(subnet_index)
            .ok_or_else(|| anyhow!("subnet 索引超出範圍: {subnet_index}"))?;
        subnet
            .get_mut("reservations")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("subnet 缺少 reservations 陣列"))
    }
}

fn parse_subnet(v: &Value) -> Result<Subnet> {
    let cidr_str = v
        .get("subnet")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("subnet 缺少 subnet 欄位"))?;
    let cidr = Ipv4Net::from_str(cidr_str).with_context(|| format!("subnet CIDR 不合法: {cidr_str}"))?;
    let pools = v
        .get("pools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.get("pool")
                        .and_then(Value::as_str)
                        .and_then(|s| parse_pool(s).ok())
                })
                .collect()
        })
        .unwrap_or_default();
    let reservations = v
        .get("reservations")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|r| parse_reservation(r).ok()).collect())
        .unwrap_or_default();
    Ok(Subnet {
        cidr,
        pools,
        reservations,
    })
}

fn parse_pool(s: &str) -> Result<IpRange> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("pool 格式不合法: {s}"))?;
    let start = Ipv4Addr::from_str(a.trim())?;
    let end = Ipv4Addr::from_str(b.trim())?;
    Ok(IpRange { start, end })
}

fn parse_reservation(v: &Value) -> Result<Reservation> {
    let hw_address = v
        .get("hw-address")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("reservation 缺少 hw-address"))?
        .to_string();
    let ip_address = Ipv4Addr::from_str(
        v.get("ip-address")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("reservation 缺少 ip-address"))?,
    )?;
    let hostname = v.get("hostname").and_then(Value::as_str).map(str::to_string);
    Ok(Reservation {
        hw_address,
        ip_address,
        hostname,
    })
}

fn reservation_to_value(r: &Reservation) -> Value {
    let mut m = Map::new();
    m.insert("hw-address".into(), Value::String(r.hw_address.clone()));
    m.insert("ip-address".into(), Value::String(r.ip_address.to_string()));
    if let Some(h) = r.hostname.as_ref().filter(|h| !h.is_empty()) {
        m.insert("hostname".into(), Value::String(h.clone()));
    }
    Value::Object(m)
}

/// 把 reservation 的已建模欄位套用到既有物件上，**保留未建模欄位**（ADR 0001 硬規則）。
fn apply_reservation_to_value(slot: &mut Value, r: &Reservation) {
    let obj = match slot.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };
    obj.insert("hw-address".into(), Value::String(r.hw_address.clone()));
    obj.insert("ip-address".into(), Value::String(r.ip_address.to_string()));
    match r.hostname.as_ref().filter(|h| !h.is_empty()) {
        Some(h) => {
            obj.insert("hostname".into(), Value::String(h.clone()));
        }
        None => {
            obj.remove("hostname");
        }
    }
}

/// 以 3 空格縮排序列化（與既有設定檔排版一致），不保留註解。
pub fn serialize_pretty3(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, 0, &mut out);
    out
}

fn write_value(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let n = map.len();
            for (i, (k, val)) in map.iter().enumerate() {
                indent(depth + 1, out);
                out.push('"');
                out.push_str(k);
                out.push_str("\": ");
                write_value(val, depth + 1, out);
                if i + 1 < n {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(depth, out);
            out.push('}');
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            let n = arr.len();
            for (i, val) in arr.iter().enumerate() {
                indent(depth + 1, out);
                write_value(val, depth + 1, out);
                if i + 1 < n {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(depth, out);
            out.push(']');
        }
        _ => out.push_str(&serde_json::to_string(v).expect("scalar serializes")),
    }
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("   ");
    }
}

/// 寫回前備份：`{檔名}.{unix秒}.bak`，保留最近 keep 份。
pub fn backup(path: &Path, keep: usize) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("取得時間失敗: {e}"))?
        .as_secs();
    let fname = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let bak = path.with_file_name(format!("{fname}.{ts}.bak"));
    std::fs::copy(path, &bak).with_context(|| format!("建立備份失敗: {}", bak.display()))?;

    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let prefix = format!("{fname}.");
    let mut backups: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ts = n
                .strip_prefix(&prefix)?
                .strip_suffix(".bak")?
                .parse::<u64>()
                .ok()?;
            Some((ts, p))
        })
        .collect();
    backups.sort_by_key(|(ts, _)| *ts);
    while backups.len() > keep {
        if let Some((_, old)) = backups.first() {
            let _ = std::fs::remove_file(old);
            backups.remove(0);
        }
    }
    Ok(Some(bak))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn fixture() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/kea-dhcp4.conf")
    }

    #[test]
    fn update_preserves_unmodeled_fields() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        f.root
            .get_mut("Dhcp4")
            .and_then(|d| d.get_mut("subnet4"))
            .and_then(Value::as_array_mut)
            .unwrap()[0]
            .get_mut("reservations")
            .and_then(Value::as_array_mut)
            .unwrap()[0]
            .as_object_mut()
            .unwrap()
            .insert("client-classes".into(), Value::String("voip".into()));
        let upd = Reservation {
            hw_address: "1c:69:7a:77:3b:98".into(),
            ip_address: Ipv4Addr::from_str("10.1.1.99").unwrap(),
            hostname: Some("renamed".into()),
        };
        f.update_reservation(0, 0, &upd).unwrap();
        let saved = f.root["Dhcp4"]["subnet4"][0]["reservations"][0].clone();
        assert_eq!(saved["client-classes"], "voip", "未建模欄位必須保留");
        assert_eq!(saved["hw-address"], "1c:69:7a:77:3b:98");
        assert_eq!(saved["ip-address"], "10.1.1.99");
        assert_eq!(saved["hostname"], "renamed");
    }

    #[test]
    fn update_removes_hostname_when_cleared() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        let upd = Reservation {
            hw_address: "1c:69:7a:77:3b:98".into(),
            ip_address: Ipv4Addr::from_str("10.1.1.11").unwrap(),
            hostname: None,
        };
        f.update_reservation(0, 0, &upd).unwrap();
        let saved = f.root["Dhcp4"]["subnet4"][0]["reservations"][0].clone();
        assert!(saved.get("hostname").is_none());
    }

    #[test]
    fn load_fixture_parses_subnets_and_reservations() {
        let f = KeaFile::load(Path::new(fixture())).expect("load fixture");
        assert_eq!(f.subnet_count(), 1);
        let s = f.subnet(0).expect("subnet 0");
        assert_eq!(s.cidr.to_string(), "10.1.0.0/16");
        assert_eq!(s.reservations.len(), 239);
        assert_eq!(s.pools.len(), 1);
        assert_eq!(s.pools[0].start, Ipv4Addr::from_str("10.1.11.1").unwrap());
        assert_eq!(s.pools[0].end, Ipv4Addr::from_str("10.1.11.250").unwrap());
        let first = &s.reservations[0];
        assert_eq!(first.hw_address, "1c:69:7a:77:3b:98");
        assert_eq!(first.ip_address.to_string(), "10.1.1.11");
        assert_eq!(first.hostname.as_deref(), Some("chufang-mg4670-3"));
    }

    #[test]
    fn serialize_uses_three_space_indent_and_roundtrips() {
        let v: Value = serde_json::from_str(r#"{"Dhcp4":{"a":1,"arr":[{"b":2}]}}"#).unwrap();
        let out = serialize_pretty3(&v);
        assert_eq!(out, "{\n   \"Dhcp4\": {\n      \"a\": 1,\n      \"arr\": [\n         {\n            \"b\": 2\n         }\n      ]\n   }\n}");
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn full_fixture_roundtrips_through_serializer() {
        let f = KeaFile::load(Path::new(fixture())).unwrap();
        let out = serialize_pretty3(&f.root);
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, f.root);
        assert!(out.starts_with("{\n   \"Dhcp4\": {\n"));
    }

    #[test]
    fn add_update_delete_mutate_model() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        let new = Reservation {
            hw_address: "aa:bb:cc:dd:ee:01".into(),
            ip_address: Ipv4Addr::from_str("10.1.9.9").unwrap(),
            hostname: Some("test-host".into()),
        };
        f.add_reservation(0, &new).unwrap();
        assert_eq!(f.subnet(0).unwrap().reservations.len(), 240);

        let upd = Reservation {
            hw_address: "aa:bb:cc:dd:ee:02".into(),
            ip_address: Ipv4Addr::from_str("10.1.9.10").unwrap(),
            hostname: None,
        };
        f.update_reservation(0, 0, &upd).unwrap();
        let s = f.subnet(0).unwrap();
        assert_eq!(s.reservations[0].hw_address, "aa:bb:cc:dd:ee:02");
        assert_eq!(s.reservations[0].hostname, None);

        f.delete_reservation(0, 0).unwrap();
        assert_eq!(f.subnet(0).unwrap().reservations.len(), 239);
    }

    #[test]
    fn save_writes_parseable_file() {
        let dir = std::env::temp_dir().join(format!("kealight-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.conf");
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        f.add_reservation(
            0,
            &Reservation {
                hw_address: "aa:bb:cc:dd:ee:ff".into(),
                ip_address: Ipv4Addr::from_str("10.1.8.8").unwrap(),
                hostname: Some("saved".into()),
            },
        )
        .unwrap();
        f.save(&p).unwrap();
        let reloaded = KeaFile::load(&p).unwrap();
        assert_eq!(reloaded.subnet(0).unwrap().reservations.len(), 240);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_creates_and_prunes() {
        let dir = std::env::temp_dir().join(format!("kealight-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kea.conf");
        std::fs::write(&p, "{}").unwrap();
        for i in 1..=3 {
            std::fs::write(dir.join(format!("kea.conf.{i}.bak")), "{}").unwrap();
        }
        let created = backup(&p, 2).unwrap().expect("backup created");
        assert!(created.exists());
        let remain: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".bak"))
            .collect();
        assert_eq!(remain.len(), 2, "保留 2 份: {remain:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_socket_read_from_fixture() {
        let f = KeaFile::load(Path::new(fixture())).unwrap();
        let cs = f.control_socket().expect("control socket");
        assert_eq!(cs.socket_type, "unix");
        assert_eq!(cs.socket_name, "/run/kea/kea4-ctrl-socket");
    }

    #[test]
    fn file_stat_missing_returns_none() {
        let dir = std::env::temp_dir().join(format!("kealight-stat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nope.conf");
        assert!(file_stat(&p).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_stat_reads_size_and_mtime() {
        let dir = std::env::temp_dir().join(format!("kealight-stat2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.conf");
        std::fs::write(&p, "hello").unwrap();
        let st = file_stat(&p).unwrap().expect("stat exists");
        assert_eq!(st.size, 5);
        assert!(st.mtime_ns > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lease_db_default_path_when_memfile_without_name() {
        let f = KeaFile::load(Path::new(fixture())).unwrap();
        match f.lease_db() {
            LeaseDb::Memfile(p) => assert_eq!(p, PathBuf::from("/var/lib/kea/kea-leases4.csv")),
            other => panic!("fixture 的 memfile 未宣告 name 應回預設路徑: {other:?}"),
        }
    }

    #[test]
    fn lease_db_rejects_non_memfile() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("type".into(), Value::String("mysql".into()));
        match f.lease_db() {
            LeaseDb::Unavailable(msg) => assert!(msg.contains("非 memfile"), "msg: {msg}"),
            other => panic!("非 memfile 應回 Unavailable: {other:?}"),
        }
    }

    #[test]
    fn lease_db_bare_name_joins_default_dir() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("name".into(), Value::String("custom.csv".into()));
        match f.lease_db() {
            LeaseDb::Memfile(p) => assert_eq!(p, PathBuf::from("/var/lib/kea/custom.csv")),
            other => panic!("裸檔名應與預設目錄組合: {other:?}"),
        }
    }

    #[test]
    fn lease_db_path_with_slash_used_literally() {
        let mut f = KeaFile::load(Path::new(fixture())).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("name".into(), Value::String("/tmp/x/leases.csv".into()));
        match f.lease_db() {
            LeaseDb::Memfile(p) => assert_eq!(p, PathBuf::from("/tmp/x/leases.csv")),
            other => panic!("含 / 的 name 應照字面使用: {other:?}"),
        }
    }

    #[test]
    fn subnet_list_carries_ids() {
        let f = KeaFile::load(Path::new(fixture())).unwrap();
        let summaries = f.subnet_list();
        assert_eq!(summaries[0].id, Some(1));
        assert_eq!(summaries[0].cidr, "10.1.0.0/16");
        assert_eq!(summaries[0].reservation_count, 239);
    }

    #[test]
    fn reservation_hwaddrs_collects_all() {
        let f = KeaFile::load(Path::new(fixture())).unwrap();
        let hws = f.reservation_hwaddrs();
        assert_eq!(hws.len(), 239);
        assert!(hws.contains(&"1c:69:7a:77:3b:98".into()));
    }
}
