use std::net::Ipv4Addr;
use std::str::FromStr;

use std::collections::HashMap;

use anyhow::{bail, Result};

/// Kea memfile CSV 的一列租用（12 欄，欄序見 kea csv_lease_file4）。
#[derive(Debug, Clone)]
pub struct Lease {
    pub address: Ipv4Addr,
    pub hwaddr: Option<String>,
    pub client_id: Option<String>,
    pub hostname: Option<String>,
    pub subnet_id: Option<u32>,
    pub state: u8,
    pub valid_lifetime: u32,
    pub expire: u64,
}

/// 是否為「作用中」租用：state=0（已指派）且尚未過期。
pub fn is_active(l: &Lease, now_secs: u64) -> bool {
    l.state == 0 && l.expire > now_secs
}

/// 已過期（含 state=0 尚未回收與 state=2 已回收）。
pub fn is_expired(l: &Lease, now_secs: u64) -> bool {
    l.state == 2 || (l.state == 0 && l.expire <= now_secs)
}

/// 狀態篩選匹對：active | all | expired | declined | released；其餘視為 active。
pub fn matches_state_filter(l: &Lease, filter: &str, now_secs: u64) -> bool {
    if filter == "expired" {
        is_expired(l, now_secs)
    } else if filter == "declined" {
        l.state == 1
    } else if filter == "released" {
        l.state == 3
    } else if filter == "all" {
        true
    } else {
        is_active(l, now_secs)
    }
}

/// 表格行顯示用標籤：state=0 但已過期顯示「已過期」，其餘同 state_label。
pub fn row_state_label(l: &Lease, now_secs: u64) -> &'static str {
    if l.state == 0 && l.expire <= now_secs {
        "已過期"
    } else {
        state_label(l.state)
    }
}

/// 中文狀態標籤（state 數值語意對照 kea basicStatesToText）。
pub fn state_label(state: u8) -> &'static str {
    match state {
        0 => "已指派",
        1 => "拒絕",
        2 => "過期回收",
        3 => "已釋放",
        4 => "已註冊",
        _ => "未知",
    }
}

/// 到期時間 → 「剩餘 X 天 X 時」；已過期回 None。
pub fn remaining_text(expire: u64, now_secs: u64) -> Option<String> {
    if expire <= now_secs {
        return None;
    }
    let secs = expire - now_secs;
    Some(format!("剩餘 {} 天 {} 時", secs / 86400, (secs % 86400) / 3600))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - 48),
        b'a'..=b'f' => Some(b - 87),
        b'A'..=b'F' => Some(b - 55),
        _ => None,
    }
}

/// kea 把 CSV 特殊字元以 `&#xHH`（十六進位，無分號）轉義（目前僅 , 與 &）；還原成原字元。
fn unescape_field(s: &str) -> String {
    let bytes: Vec<u8> = s.as_bytes().into_iter().map(|b| *b).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && i + 4 < bytes.len()
            && bytes[i + 1] == b'#'
            && bytes[i + 2] == b'x'
        {
            let hi = hex_val(bytes[i + 3]);
            let lo = hex_val(bytes[i + 4]);
            if hi.is_some() && lo.is_some() {
                out.push(((hi.unwrap() << 4) + lo.unwrap()) as char);
                i += 5;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn nonempty(s: &String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 解析 memfile CSV 全文。同 address 多列（append-only）以最後一列為現況；
/// 個別壞列略過；表頭不符或完全沒有表頭 → Err。
/// 回傳順序維持首次出現的順序（CSV 順序），不依賴 hash 迭代序。
pub fn parse(text: &str) -> Result<Vec<Lease>> {
    let mut by_address: HashMap<String, usize> = HashMap::new();
    let mut leaves: Vec<Lease> = Vec::new();
    let mut saw_header = false;
    for line0 in text.split('\n') {
        let owned = line0.to_string();
        let line = owned.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<String> = line
            .split(',')
            .map(|p| unescape_field(p.as_ref()))
            .collect();
        if !saw_header {
            if parts.len() != 12 || parts[0] != "address" {
                bail!("租用檔表頭不合法（需 12 欄，首欄 address）");
            }
            saw_header = true;
            continue;
        }
        if parts.len() != 12 {
            continue;
        }
        let Ok(address) = Ipv4Addr::from_str(parts[0].trim()) else {
            continue;
        };
        let Ok(valid_lifetime) = parts[3].parse::<u32>() else {
            continue;
        };
        let Ok(expire) = parts[4].parse::<u64>() else {
            continue;
        };
        let subnet_id = parts[5].trim().parse::<u32>().ok();
        let Ok(raw_state) = parts[9].parse::<u64>() else {
            continue;
        };
        let Ok(state) = u8::try_from(raw_state) else {
            continue;
        };
        let key = parts[0].trim().to_string();
        let lease = Lease {
            address,
            hwaddr: nonempty(&parts[1]),
            client_id: nonempty(&parts[2]),
            hostname: nonempty(&parts[8]),
            subnet_id,
            state,
            valid_lifetime,
            expire,
        };
        match by_address.get(&key) {
            Some(i) => leaves[*i] = lease,
            None => {
                by_address.insert(key, leaves.len());
                leaves.push(lease);
            }
        }
    }
    if !saw_header {
        bail!("租用檔內容為空（缺少表頭）");
    }
    Ok(leaves)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_row() {
        let text = concat!(
            "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id",
            "\n",
            "10.1.9.9,aa:bb:cc:dd:ee:01,,3600,1900000000,1,1,1,host-a,0,{},0",
        );
        let leases = parse(text).unwrap();
        assert_eq!(leases.len(), 1);
        let l = &leases[0];
        assert_eq!(l.address.to_string(), "10.1.9.9");
        assert_eq!(l.hwaddr, Some("aa:bb:cc:dd:ee:01".into()));
        assert_eq!(l.client_id, None);
        assert_eq!(l.hostname, Some("host-a".into()));
        assert_eq!(l.subnet_id, Some(1));
        assert_eq!(l.state, 0);
        assert_eq!(l.valid_lifetime, 3600);
        assert_eq!(l.expire, 1900000000);
    }

    #[test]
    fn unescapes_kea_entities_in_fields() {
        let text = concat!(
            "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id",
            "\n",
            "10.1.9.9,aa:bb:cc:dd:ee:01,,3600,1900000000,1,1,1,",
            "three&#x2cexample&#x2ccom,",
            "0,{ \"a\": 1&#x2c \"b\": \"c\" },0",
        );
        let leases = parse(text).unwrap();
        assert_eq!(leases[0].hostname, Some("three,example,com".into()));
    }

    #[test]
    fn dedup_by_address_keeps_last_row() {
        let text = concat!(
            "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id",
            "\n",
            "10.1.9.9,aa:bb:cc:dd:ee:01,,3600,1900000000,1,1,1,old,0,,0\n",
            "10.1.9.9,aa:bb:cc:dd:ee:02,,3600,1900000000,1,1,1,new,0,,0",
        );
        let leases = parse(text).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].hwaddr, Some("aa:bb:cc:dd:ee:02".into()));
        assert_eq!(leases[0].hostname, Some("new".into()));
    }

    #[test]
    fn bad_rows_skipped_good_rows_kept() {
        let text = concat!(
            "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id",
            "\n",
            "not-an-ip,,,3600,1900000000,1,1,1,bad,0,,0\n",   // address 爛
            "a,b,c\n",                                          // 欄數不足
            "10.1.9.9,aa:bb:cc:dd:ee:01,,3600,1900000000,1,1,1,ok,0,,0",
        );
        let leases = parse(text).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].hostname, Some("ok".into()));
    }

    #[test]
    fn header_with_extra_fields_rejected() {
        let text = "a,b,c\n1,2,3";
        assert!(parse(text).is_err(), "表頭不合 12 欄應拒絕");
        assert!(parse("").is_err(), "空檔應拒絕");
    }

    #[test]
    fn active_requires_state_zero_and_future_expire() {
        let base = Lease {
            address: Ipv4Addr::from_str("10.1.9.9").unwrap(),
            hwaddr: None,
            client_id: None,
            hostname: None,
            subnet_id: None,
            state: 0,
            valid_lifetime: 0,
            expire: 2000,
        };
        assert!(is_active(&base, 1000));
        let mut expired = base.clone();
        expired.expire = 1000;
        assert!(!is_active(&expired, 1000), "expire == now 不算作用中");
        assert!(!is_active(&expired, 2000), "過期不算作用中");
        let mut declined = base.clone();
        declined.state = 1;
        assert!(!is_active(&declined, 1000), "declined 不算作用中");
    }

    #[test]
    fn remaining_formats_days_and_hours() {
        assert_eq!(remaining_text(1000, 900), Some("剩餘 0 天 0 時".into()));
        assert_eq!(remaining_text(90000, 0), Some("剩餘 1 天 1 時".into()));
        assert!(remaining_text(100, 200).is_none(), "已過期回 None");
    }
}