use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::str::FromStr;

use ipnet::Ipv4Net;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub hw_address: String,
    pub ip_address: Ipv4Addr,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpRange {
    pub start: Ipv4Addr,
    pub end: Ipv4Addr,
}

impl IpRange {
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let ip = u32::from(ip);
        u32::from(self.start) <= ip && ip <= u32::from(self.end)
    }
}

#[derive(Debug, Clone)]
pub struct Subnet {
    pub cidr: Ipv4Net,
    pub pools: Vec<IpRange>,
    pub reservations: Vec<Reservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    DuplicateHwAddress,
    IpInUse,
    IpInPool,
    IpOutOfSubnet,
}

/// 檢查 candidate（新增或修改 edit_index 那一筆）與 subnet 現況的衝突。
/// 回傳所有衝突；空陣列代表可寫入。
pub fn check_conflicts(
    subnet: &Subnet,
    candidate: &Reservation,
    edit_index: Option<usize>,
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    if !subnet.cidr.contains(&candidate.ip_address) {
        conflicts.push(Conflict::IpOutOfSubnet);
    }
    if subnet.pools.iter().any(|p| p.contains(candidate.ip_address)) {
        conflicts.push(Conflict::IpInPool);
    }
    conflicts.extend(reservation_conflicts(&subnet.reservations, candidate, edit_index));
    conflicts
}

/// 對照一組既有 reservation（不含 edit_index 自身）的 hw-address／ip-address 衝突。
fn reservation_conflicts(
    reservations: &[Reservation],
    candidate: &Reservation,
    edit_index: Option<usize>,
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    for (i, r) in reservations.iter().enumerate() {
        if Some(i) == edit_index {
            continue;
        }
        if r.hw_address.eq_ignore_ascii_case(&candidate.hw_address) {
            conflicts.push(Conflict::DuplicateHwAddress);
        }
        if r.ip_address == candidate.ip_address {
            conflicts.push(Conflict::IpInUse);
        }
    }
    conflicts
}

/// 衝突的中文原因（供 UI 顯示）。
pub fn conflict_reasons(c: &[Conflict]) -> Vec<String> {
    c.iter()
        .map(|x| match x {
            Conflict::DuplicateHwAddress => "hw-address 已存在（唯一鍵衝突）".to_string(),
            Conflict::IpInUse => "IP 已被其他 reservation 佔用".to_string(),
            Conflict::IpInPool => "IP 落在 dynamic pool 範圍內".to_string(),
            Conflict::IpOutOfSubnet => "IP 超出 subnet 範圍".to_string(),
        })
        .collect()
}

/// hostname 重複只是警告（不擋寫入）。若候選的 hostname 已存在於其他 reservation，
/// 回傳該 hostname（供 UI 顯示警告）。
pub fn duplicate_hostnames(subnet: &Subnet, candidate: &Reservation, edit_index: Option<usize>) -> Vec<String> {
    let mut dupes = Vec::new();
    let Some(cand) = candidate.hostname.as_deref() else {
        return dupes;
    };
    if cand.is_empty() {
        return dupes;
    }
    for (i, r) in subnet.reservations.iter().enumerate() {
        if Some(i) == edit_index {
            continue;
        }
        if r.hostname.as_deref() == Some(cand) {
            dupes.push(cand.to_string());
            break;
        }
    }
    dupes
}

/// 貼上文字逐列的原始欄位（trim 後），供預覽顯示。
#[derive(Debug, Clone)]
pub struct RawPasteColumns {
    pub hw: String,
    pub ip: String,
    pub hostname: String,
}

/// 貼上文字的單列解析結果。
#[derive(Debug, Clone)]
pub enum PasteLine {
    /// 首欄不是 MAC 格式（可能是表頭或不完整列），靜默跳過。
    NotMac,
    /// 缺或無效的欄位，附跳過原因。
    Invalid { line_no: usize, raw: RawPasteColumns, reason: String },
    /// 可用的候選筆。
    Candidate { line_no: usize, reservation: Reservation, raw: RawPasteColumns },
}

/// 預覽列的最終處置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteAction {
    Add,
    Update,
    NoChange,
    Skip,
}

/// 快貼預覽的單列結果。
#[derive(Debug, Clone)]
pub struct PasteOutcome {
    pub line_no: usize,
    pub action: PasteAction,
    /// Skip 時也保留候選值供顯示；缺欄所致而無候選值時為 None。
    pub reservation: Option<Reservation>,
    pub raw: RawPasteColumns,
    pub detail: Option<String>,
}

/// 快貼的實際寫入動作。
#[derive(Debug, Clone)]
pub enum PasteMutation {
    Update { index: usize, reservation: Reservation },
    Add { reservation: Reservation },
}

/// 一次性快貼的完整計畫（預覽與寫入共用同一份）。
#[derive(Debug, Clone)]
pub struct PastePlan {
    pub outcomes: Vec<PasteOutcome>,
    /// 首欄非 MAC 格式而被靜默跳過的列數。
    pub silent_skipped: usize,
    pub mutations: Vec<PasteMutation>,
    /// 套用後仍重複的 hostname（僅警告）。
    pub warn_hostnames: Vec<String>,
}

/// 快貼接受的 MAC 格式較寬：`:` 或 `-` 分隔、或 12 碼無分隔（亦可夾雜空白）。
/// 一律正規化為小寫冒號格式；否則回傳 None。
pub fn normalize_mac_paste(s: &str) -> Option<String> {
    let t = s.trim();
    let sep = if t.contains(':') {
        ':'
    } else if t.contains('-') {
        '-'
    } else {
        let hex: Vec<char> = t.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 12 {
            return Some(
                hex.chunks(2)
                    .map(|pair| pair.iter().collect::<String>())
                    .collect::<Vec<String>>()
                    .join(":")
                    .to_lowercase(),
            );
        }
        return None;
    };
    let parts: Vec<&str> = t.split(sep).collect();
    if parts.len() != 6
        || parts.iter().any(|p| p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(parts.join(":").to_lowercase())
}

/// 解析貼上的多列文字：每列一筆，欄位以 Tab（該列無 Tab 時退回逗號）分隔，
/// 固定順序 hw-address → ip-address → hostname；多餘欄位忽略。
/// 空列跳過；首欄非 MAC 格式（推測為表頭或不完整列）靜默跳過。
pub fn parse_paste_lines(raw: &str) -> Vec<PasteLine> {
    let mut out = Vec::new();
    for (i, line) in raw.split('\n').enumerate() {
        let line_no = i + 1;
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<String> = if line.contains('\t') {
            line.split('\t').map(|c| c.trim().to_string()).collect()
        } else {
            line.split(',').map(|c| c.trim().to_string()).collect()
        };
        let hw = cols.first().cloned().unwrap_or_default();
        let ip = cols.get(1).cloned().unwrap_or_default();
        let hostname = cols.get(2).cloned().unwrap_or_default();
        let raw_cols = RawPasteColumns { hw: hw.clone(), ip: ip.clone(), hostname: hostname.clone() };
        let Some(normalized_hw) = normalize_mac_paste(&hw) else {
            out.push(PasteLine::NotMac);
            continue;
        };
        let ip_address = match Ipv4Addr::from_str(&ip) {
            Ok(v) => v,
            Err(_) => {
                let reason = if ip.is_empty() {
                    "缺少 ip-address".to_string()
                } else {
                    format!("ip-address 格式不合法: {}", ip)
                };
                out.push(PasteLine::Invalid { line_no, raw: raw_cols, reason });
                continue;
            }
        };
        let hostname = if hostname.is_empty() { None } else { Some(hostname.clone()) };
        out.push(PasteLine::Candidate {
            line_no,
            reservation: Reservation { hw_address: normalized_hw, ip_address, hostname },
            raw: raw_cols,
        });
    }
    out
}

/// 規劃整批快貼：解析 → 依 hw-address 收斂重複（最後一筆勝出）→ 依列序逐筆
/// 評估衝突套用。hw-address 重複不構成衝突（upsert 鍵）；其餘衝突（IP 被佔用、
/// 落入 pool、超出 subnet）則跳過該筆。套用順序會影響批次內同 IP 誰先誰後。
pub fn plan_paste(subnet: &Subnet, raw: &str) -> PastePlan {
    let mut silent_skipped = 0usize;
    let mut invalid: Vec<PasteOutcome> = Vec::new();
    let mut keep: HashMap<String, (usize, Reservation, RawPasteColumns)> = HashMap::new();
    for line in parse_paste_lines(raw) {
        match line {
            PasteLine::NotMac => silent_skipped += 1,
            PasteLine::Invalid { line_no, raw, reason } => invalid.push(PasteOutcome {
                line_no,
                action: PasteAction::Skip,
                reservation: None,
                raw,
                detail: Some(reason),
            }),
            PasteLine::Candidate { line_no, reservation, raw } => {
                keep.insert(reservation.hw_address.clone(), (line_no, reservation, raw));
            }
        }
    }
    let mut survivors: Vec<&(usize, Reservation, RawPasteColumns)> = keep.values().collect();
    survivors.sort_by_key(|(line_no, _, _)| *line_no);

    let mut working: Vec<Reservation> = subnet.reservations.clone();
    let mut outcomes: Vec<PasteOutcome> = Vec::new();
    let mut mutations: Vec<PasteMutation> = Vec::new();
    for (line_no, cand, raw) in survivors {
        let existing =
            working.iter().position(|r| r.hw_address.eq_ignore_ascii_case(&cand.hw_address));
        let mut conflicts = Vec::new();
        if !subnet.cidr.contains(&cand.ip_address) {
            conflicts.push(Conflict::IpOutOfSubnet);
        }
        if subnet.pools.iter().any(|p| p.contains(cand.ip_address)) {
            conflicts.push(Conflict::IpInPool);
        }
        conflicts.extend(reservation_conflicts(&working, cand, existing));
        conflicts.retain(|c| *c != Conflict::DuplicateHwAddress);
        if !conflicts.is_empty() {
            outcomes.push(PasteOutcome {
                line_no: *line_no,
                action: PasteAction::Skip,
                reservation: Some(cand.clone()),
                raw: raw.clone(),
                detail: Some(conflict_reasons(&conflicts).join("；")),
            });
            continue;
        }
        match existing {
            Some(i) => {
                let changed = working[i] != *cand;
                if changed {
                    working[i] = cand.clone();
                    mutations.push(PasteMutation::Update { index: i, reservation: cand.clone() });
                }
                outcomes.push(PasteOutcome {
                    line_no: *line_no,
                    action: if changed { PasteAction::Update } else { PasteAction::NoChange },
                    reservation: Some(cand.clone()),
                    raw: raw.clone(),
                    detail: None,
                });
            }
            None => {
                working.push(cand.clone());
                mutations.push(PasteMutation::Add { reservation: cand.clone() });
                outcomes.push(PasteOutcome {
                    line_no: *line_no,
                    action: PasteAction::Add,
                    reservation: Some(cand.clone()),
                    raw: raw.clone(),
                    detail: None,
                });
            }
        }
    }

    let mut warned: Vec<String> = Vec::new();
    for o in outcomes.iter() {
        let Some(base) = o.reservation.as_ref() else { continue };
        let Some(host) = base.hostname.as_deref() else { continue };
        if host.is_empty() {
            continue;
        }
        if matches!(o.action, PasteAction::Add | PasteAction::Update | PasteAction::NoChange)
            && working.iter().filter(|r| r.hostname.as_deref() == Some(host)).count() > 1
            && !warned.contains(&host.to_string())
        {
            warned.push(host.to_string());
        }
    }

    outcomes.extend(invalid);
    outcomes.sort_by_key(|o| o.line_no);
    PastePlan { outcomes, silent_skipped, mutations, warn_hostnames: warned }
}

#[cfg(test)]
fn cidr(s: &str) -> Ipv4Net {
    use std::str::FromStr;
    Ipv4Net::from_str(s).expect("test cidr")
}

#[cfg(test)]
fn r(hw: &str, ip: &str, hostname: Option<&str>) -> Reservation {
    use std::str::FromStr;
    Reservation {
        hw_address: hw.to_string(),
        ip_address: Ipv4Addr::from_str(ip).unwrap(),
        hostname: hostname.map(str::to_string),
    }
}

#[cfg(test)]
fn subnet() -> Subnet {
    use std::str::FromStr;
    Subnet {
        cidr: cidr("10.1.0.0/16"),
        pools: vec![IpRange {
            start: Ipv4Addr::from_str("10.1.11.1").unwrap(),
            end: Ipv4Addr::from_str("10.1.11.250").unwrap(),
        }],
        reservations: vec![
            r("1c:69:7a:77:3b:98", "10.1.1.11", Some("chufang-mg4670-3")),
            r("c0:3f:d5:b4:bb:e3", "10.1.1.13", Some("damenjingweishi")),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn no_conflict_for_fresh_unique_reservation() {
        let s = subnet();
        let cand = r("aa:bb:cc:dd:ee:ff", "10.1.2.5", Some("new-host"));
        assert!(check_conflicts(&s, &cand, None).is_empty());
    }

    #[test]
    fn duplicate_hw_address_is_a_conflict() {
        let s = subnet();
        let cand = r("1C:69:7A:77:3B:98", "10.1.2.5", None);
        assert_eq!(check_conflicts(&s, &cand, None), vec![Conflict::DuplicateHwAddress]);
    }

    #[test]
    fn ip_in_use_by_another_reservation_is_a_conflict() {
        let s = subnet();
        let cand = r("aa:bb:cc:dd:ee:ff", "10.1.1.11", None);
        assert_eq!(check_conflicts(&s, &cand, None), vec![Conflict::IpInUse]);
    }

    #[test]
    fn ip_inside_dynamic_pool_is_a_conflict() {
        let s = subnet();
        let cand = r("aa:bb:cc:dd:ee:ff", "10.1.11.100", None);
        assert_eq!(check_conflicts(&s, &cand, None), vec![Conflict::IpInPool]);
    }

    #[test]
    fn ip_outside_subnet_is_a_conflict() {
        let s = subnet();
        let cand = r("aa:bb:cc:dd:ee:ff", "192.168.1.1", None);
        assert_eq!(check_conflicts(&s, &cand, None), vec![Conflict::IpOutOfSubnet]);
    }

    #[test]
    fn editing_a_record_ignores_its_own_identity() {
        let s = subnet();
        let cand = r("1c:69:7a:77:3b:98", "10.1.1.12", Some("chufang-mg4670-3"));
        let conflicts = check_conflicts(&s, &cand, Some(0));
        assert!(conflicts.is_empty(), "editing index 0 with its own MAC/IP must not conflict: {conflicts:?}");
    }

    #[test]
    fn multiple_conflicts_reported_together() {
        let s = subnet();
        // 同時 MAC 重複 index1 且 IP 落在 pool
        let cand = r("c0:3f:d5:b4:bb:e3", "10.1.11.200", None);
        let c = check_conflicts(&s, &cand, None);
        assert!(c.contains(&Conflict::DuplicateHwAddress));
        assert!(c.contains(&Conflict::IpInPool));
    }

    #[test]
    fn ip_range_contains_boundaries() {
        let range = IpRange {
            start: Ipv4Addr::from_str("10.1.11.1").unwrap(),
            end: Ipv4Addr::from_str("10.1.11.250").unwrap(),
        };
        assert!(range.contains(Ipv4Addr::from_str("10.1.11.1").unwrap()));
        assert!(range.contains(Ipv4Addr::from_str("10.1.11.250").unwrap()));
        assert!(!range.contains(Ipv4Addr::from_str("10.1.11.0").unwrap()));
        assert!(!range.contains(Ipv4Addr::from_str("10.1.11.251").unwrap()));
    }

    #[test]
    fn duplicate_hostname_is_a_warning_not_conflict() {
        let s = subnet();
        let cand = r("aa:bb:cc:dd:ee:ff", "10.1.2.5", Some("chufang-mg4670-3"));
        assert!(check_conflicts(&s, &cand, None).is_empty());
        assert_eq!(duplicate_hostnames(&s, &cand, None), vec!["chufang-mg4670-3".to_string()]);
    }

    #[test]
    fn editing_own_hostname_is_not_a_warning() {
        let s = subnet();
        let cand = r("1c:69:7a:77:3b:98", "10.1.1.12", Some("chufang-mg4670-3"));
        assert!(duplicate_hostnames(&s, &cand, Some(0)).is_empty());
    }

    #[test]
    fn paste_normalizes_dash_bare_and_colon_macs() {
        assert_eq!(normalize_mac_paste("AA-BB-CC-DD-EE-08"), Some("aa:bb:cc:dd:ee:08".into()));
        assert_eq!(normalize_mac_paste("AABBCCDDEEFF"), Some("aa:bb:cc:dd:ee:ff".into()));
        assert_eq!(normalize_mac_paste("AA BB CC DD EE FF"), Some("aa:bb:cc:dd:ee:ff".into()));
        assert_eq!(normalize_mac_paste("aa:bb:cc:dd:ee:01"), Some("aa:bb:cc:dd:ee:01".into()));
        assert_eq!(normalize_mac_paste("not-a-mac"), None);
        assert_eq!(normalize_mac_paste("aabbccddeeff01"), None);
    }

    #[test]
    fn parse_paste_lines_handles_tab_comma_crlf_and_extras() {
        let raw = "aa:bb:cc:dd:ee:a1\t10.1.9.6\thost\r\n\
                   aa:bb:cc:dd:ee:a2,10.1.9.7,other\n\
                   aa:bb:cc:dd:ee:a3\t10.1.9.8\thost\textra\n\
                   \n";
        let lines = parse_paste_lines(raw);
        let candidates: Vec<_> = lines
            .iter()
            .filter_map(|l| match l {
                PasteLine::Candidate { reservation, .. } => Some(reservation.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(candidates.len(), 3, "Tab、逗號退回、CRLF、多餘欄位、空列皆應解析：{lines:?}");
        assert_eq!(candidates[0].hostname.as_deref(), Some("host"));
        assert_eq!(candidates[1].hw_address, "aa:bb:cc:dd:ee:a2");
        assert_eq!(candidates[2].hostname.as_deref(), Some("host"));
    }

    #[test]
    fn parse_paste_lines_skips_header_and_invalid_rows() {
        let raw = "hw-address\tip-address\thostname\n\
                   aa:bb:cc:dd:ee:b1\tbad-ip\tx\n\
                   aa:bb:cc:dd:ee:b2\t\tx\n\
                   aa:bb:cc:dd:ee:b3\t10.1.9.9\tok\n";
        let lines = parse_paste_lines(raw);
        assert!(matches!(lines[0], PasteLine::NotMac), "表頭首欄非 MAC 應靜默跳過");
        assert!(matches!(&lines[1], PasteLine::Invalid { reason, .. } if reason.contains("格式不合法")));
        assert!(matches!(&lines[2], PasteLine::Invalid { reason, .. } if reason.contains("缺少 ip-address")));
        assert!(matches!(&lines[3], PasteLine::Candidate { .. }));
    }

    #[test]
    fn plan_paste_adds_new_and_updates_existing() {
        let s = subnet();
        let raw = "aa:bb:cc:dd:ee:99\t10.1.9.99\tbox1\n\
                   1c:69:7a:77:3b:98\t10.1.1.99\trenamed\n\
                   c0:3f:d5:b4:bb:e3\t10.1.1.13\tdamenjingweishi\n";
        let plan = plan_paste(&s, raw);
        let actions: Vec<_> = plan.outcomes.iter().map(|o| o.action).collect();
        assert_eq!(actions, vec![PasteAction::Add, PasteAction::Update, PasteAction::NoChange]);
        assert_eq!(plan.mutations.len(), 2);
        let mut added = None;
        let mut updated = None;
        for m in plan.mutations {
            match m {
                PasteMutation::Add { reservation } => added = Some(reservation),
                PasteMutation::Update { index, reservation } => updated = Some((index, reservation)),
            }
        }
        assert_eq!(added.unwrap().hw_address, "aa:bb:cc:dd:ee:99");
        let (idx, r) = updated.unwrap();
        assert_eq!(idx, 0);
        assert_eq!(r.ip_address.to_string(), "10.1.1.99");
        assert_eq!(r.hostname.as_deref(), Some("renamed"));
    }

    #[test]
    fn plan_paste_dedupes_batch_by_mac_last_wins() {
        let s = subnet();
        let raw = "aa:bb:cc:dd:ee:97\t10.1.9.97\tfirst\n\
                   aa:bb:cc:dd:ee:97\t10.1.9.98\tsecond\n";
        let plan = plan_paste(&s, raw);
        assert_eq!(plan.outcomes.len(), 1, "同 MAC 兩列應收斂成一筆：{plan:?}");
        assert_eq!(plan.mutations.len(), 1);
        match &plan.mutations[0] {
            PasteMutation::Add { reservation } => {
                assert_eq!(reservation.ip_address.to_string(), "10.1.9.98");
                assert_eq!(reservation.hostname.as_deref(), Some("second"));
            }
            other => panic!("應為新增：{other:?}"),
        }
    }

    #[test]
    fn plan_paste_skips_conflicting_ips() {
        let s = subnet();
        let raw = "aa:bb:cc:dd:ee:90\t10.1.9.90\tok\n\
                   aa:bb:cc:dd:ee:91\t10.1.1.11\ttaken\n\
                   aa:bb:cc:dd:ee:92\t10.1.11.100\tpooled\n\
                   aa:bb:cc:dd:ee:93\t192.168.1.1\toutside\n";
        let plan = plan_paste(&s, raw);
        let skips = plan.outcomes.iter().filter(|o| o.action == PasteAction::Skip).count();
        assert_eq!(skips, 3, "IP 被佔用／pool／範圍外應跳過：{plan:?}");
        assert_eq!(plan.mutations.len(), 1);
        let detail = plan.outcomes[1].detail.as_deref().unwrap();
        assert!(detail.contains("已被其他 reservation 佔用"), "detail: {detail}");
    }

    #[test]
    fn plan_paste_intra_batch_ip_conflict_second_is_skipped() {
        let s = subnet();
        let raw = "aa:bb:cc:dd:ee:80\t10.1.9.80\tbox1\n\
                   aa:bb:cc:dd:ee:81\t10.1.9.80\tbox2\n";
        let plan = plan_paste(&s, raw);
        assert_eq!(plan.mutations.len(), 1);
        assert_eq!(plan.outcomes.len(), 2);
        assert_eq!(plan.outcomes[0].action, PasteAction::Add);
        assert_eq!(plan.outcomes[1].action, PasteAction::Skip, "同 IP 後筆應被跳過：{plan:?}");
    }

    #[test]
    fn plan_paste_empty_hostname_clears_on_update() {
        let s = subnet();
        let raw = "1c:69:7a:77:3b:98\t10.1.1.11\t\n";
        let plan = plan_paste(&s, raw);
        assert_eq!(plan.mutations.len(), 1);
        match &plan.mutations[0] {
            PasteMutation::Update { index, reservation } => {
                assert_eq!(*index, 0);
                assert_eq!(reservation.hostname, None);
            }
            other => panic!("空 hostname 覆寫應為更新：{other:?}"),
        }
    }

    #[test]
    fn plan_paste_reports_duplicate_hostnames_as_warning() {
        let s = subnet();
        let raw = "aa:bb:cc:dd:ee:70\t10.1.9.70\tchufang-mg4670-3\n";
        let plan = plan_paste(&s, raw);
        assert_eq!(plan.mutations.len(), 1);
        assert!(plan.warn_hostnames.contains(&"chufang-mg4670-3".to_string()));
    }

    #[test]
    fn plan_paste_counts_silent_header_skips() {
        let s = subnet();
        let raw = "hw-address\tip-address\thostname\n\
                   aa:bb:cc:dd:ee:60\t10.1.9.60\tok\n";
        let plan = plan_paste(&s, raw);
        assert_eq!(plan.silent_skipped, 1);
        assert_eq!(plan.mutations.len(), 1);
    }
}
