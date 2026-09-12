use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};

use crate::file::ControlSocket;

/// 對 kea control-socket 發送 config-reload，回傳 kea 的回應文字。
/// 依設定：unix socket 直連 / TCP（host:port）。
pub fn reload(cs: &ControlSocket) -> Result<String> {
    let cmd = serde_json::json!({ "command": "config-reload" });
    let response = match cs.socket_type.as_str() {
        "unix" => {
            let mut stream = std::os::unix::net::UnixStream::connect(&cs.socket_name)
                .with_context(|| format!("無法連線 control socket: {}", cs.socket_name))?;
            exchange(&mut stream, &cmd)?
        }
        "tcp" => {
            let mut stream = std::net::TcpStream::connect(&cs.socket_name)
                .with_context(|| format!("無法連線 control socket: {}", cs.socket_name))?;
            exchange(&mut stream, &cmd)?
        }
        other => bail!("不支援的 control-socket 型別: {other}"),
    };

    let parsed: serde_json::Value = serde_json::from_str(&response)
        .context("kea 回應不是合法 JSON")?;
    let result = parsed
        .get("result")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("kea 回應缺少 result: {response}"))?;
    if result != 0 {
        let text = parsed.get("text").and_then(serde_json::Value::as_str).unwrap_or(&response);
        bail!("kea config-reload 失敗 (result={result}): {text}");
    }
    Ok(parsed
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ok")
        .to_string())
}

fn exchange<S: Read + Write>(stream: &mut S, cmd: &serde_json::Value) -> Result<String> {
    let body = serde_json::to_vec(cmd)?;
    stream.write_all(&body).context("寫入 control socket 失敗")?;
    stream.flush().ok();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).context("讀取 kea 回應失敗")?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}
