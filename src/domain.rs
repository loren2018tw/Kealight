use std::net::Ipv4Addr;

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
    for (i, r) in subnet.reservations.iter().enumerate() {
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
}
