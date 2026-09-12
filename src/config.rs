use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct KealightConfig {
    /// kea 設定檔路徑（必填）
    pub kea_config: PathBuf,
    /// HTTP 綁定位址
    #[serde(default = "default_bind")]
    pub bind: String,
    /// HTTP 埠（預設 7777）
    #[serde(default = "default_port")]
    pub port: u16,
    /// 寫回前保留的備份份數
    #[serde(default = "default_backup_keep")]
    pub backup_keep: usize,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    7777
}

fn default_backup_keep() -> usize {
    10
}

pub fn load(path: &std::path::Path) -> Result<KealightConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("讀取設定檔失敗: {}", path.display()))?;
    let cfg: KealightConfig =
        toml::from_str(&text).with_context(|| format!("設定檔格式不合法: {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let toml = r#"
kea_config = "testdata/kea-dhcp4.conf"
bind = "0.0.0.0"
port = 9000
backup_keep = 5
"#;
        let dir = std::env::temp_dir().join(format!("kealight-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kealight.toml");
        std::fs::write(&p, toml).unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.kea_config, PathBuf::from("testdata/kea-dhcp4.conf"));
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.backup_keep, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_applied_for_missing_fields() {
        let toml = "kea_config = \"kea.conf\"\n";
        let dir = std::env::temp_dir().join(format!("kealight-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kealight.toml");
        std::fs::write(&p, toml).unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, 7777);
        assert_eq!(cfg.backup_keep, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_kea_config_is_an_error() {
        let dir = std::env::temp_dir().join(format!("kealight-cfg3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kealight.toml");
        std::fs::write(&p, "port = 1\n").unwrap();
        assert!(load(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
