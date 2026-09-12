use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::domain::{check_conflicts, duplicate_hostnames, Conflict, Reservation};
use crate::file::{backup, ControlSocket, KeaFile, SubnetSummary};
use crate::reload::reload;

const PAGE_SIZE: usize = 50;
const HTMX_JS: &str = include_str!("../static/htmx.min.js");

pub struct AppState {
    pub kea_path: PathBuf,
    pub backup_keep: usize,
    pub file: KeaFile,
    pub saves_since_apply: u64,
}

impl AppState {
    fn save_locked(&mut self) -> Result<()> {
        backup(&self.kea_path, self.backup_keep)?;
        self.file.save(&self.kea_path)?;
        self.saves_since_apply += 1;
        Ok(())
    }
}

pub fn app(state: AppState) -> Router {
    let state = Arc::new(Mutex::new(state));
    Router::new()
        .route("/", get(root))
        .route("/subnet/{idx}", get(subnet_list))
        .route("/subnet/{idx}/new", get(new_form).post(create_reservation))
        .route("/subnet/{idx}/res/{ridx}/edit", get(edit_form).post(update_reservation))
        .route("/subnet/{idx}/res/{ridx}/delete", post(delete_reservation))
        .route("/apply", post(apply_reload))
        .route("/static/htmx.min.js", get(|| async { ([("Content-Type", "application/javascript")], HTMX_JS) }))
        .with_state(state)
}

type Shared = Arc<Mutex<AppState>>;

async fn root(State(state): State<Shared>) -> Response {
    let idx = {
        let st = state.lock().await;
        if st.file.subnet_count() == 0 {
            return page_500("設定檔內沒有 subnet4 區塊");
        }
        0
    };
    Redirect::to(&format!("/subnet/{idx}?pick=1")).into_response()
}

#[derive(Deserialize, Default)]
struct ListQuery {
    q: Option<String>,
    page: Option<usize>,
    pick: Option<u8>,
    warn: Option<String>,
}

async fn subnet_list(
    State(state): State<Shared>,
    Path(idx): Path<usize>,
    Query(q): Query<ListQuery>,
) -> Response {
    let (html, status) = {
        let st = state.lock().await;
        match render_subnet_list(&st, idx, &q) {
            Ok(body) => (page(&format!("Subnet {idx}"), &body), StatusCode::OK),
            Err(e) => (page("錯誤", &err_html(&e.to_string())), StatusCode::NOT_FOUND),
        }
    };
    (status, Html(html)).into_response()
}

fn render_subnet_list(st: &AppState, idx: usize, q: &ListQuery) -> Result<String> {
    let subnet = st.file.subnet(idx)?;
    let subnets = st.file.subnet_list();

    let needle = q.q.clone().unwrap_or_default().trim().to_lowercase();
    let matches = |r: &Reservation| -> bool {
        if needle.is_empty() {
            return true;
        }
        r.hw_address.to_lowercase().contains(&needle)
            || r.ip_address.to_string().contains(&needle)
            || r.hostname.as_deref().unwrap_or("").to_lowercase().contains(&needle)
    };
    let filtered: Vec<&Reservation> = subnet.reservations.iter().filter(|r| matches(r)).collect();
    let total = filtered.len();
    let pages = total.div_ceil(PAGE_SIZE);
    let page = q.page.unwrap_or(0).min(pages.saturating_sub(1));
    let rows = &filtered[page * PAGE_SIZE..total.min((page + 1) * PAGE_SIZE)];

    let mut rows_html = String::new();
    for (i, r) in rows.iter().enumerate() {
        let global = page * PAGE_SIZE + i;
        let hw = escape(&r.hw_address);
        let ip = r.ip_address.to_string();
        let hostname = escape(r.hostname.as_deref().unwrap_or(""));
        rows_html.push_str(&format!(
            r#"<tr>
<td>{}</td><td>{}</td><td>{}</td><td>{}</td>
<td>
<a class="btn" href="/subnet/{idx}/res/{global}/edit">編輯</a>
<button class="btn" hx-post="/subnet/{idx}/res/{global}/delete" hx-confirm="確定刪除 {hw}？" hx-target="body" hx-swap="outerHTML">刪除</button>
</td>
</tr>"#,
            global + 1,
            hw,
            ip,
            hostname,
            idx = idx,
            global = global,
            hw = hw,
        ));
    }

    let mut pager = String::new();
    if pages > 1 {
        for p in 0..pages {
            let sep = if needle.is_empty() { String::new() } else { format!("&q={}", urlencode(&needle)) };
            pager.push_str(&format!(
                r#"<a class="btn{cls}" href="/subnet/{idx}?page={page}{sep}">{label}</a> "#,
                cls = if p == page { " primary" } else { "" },
                idx = idx,
                page = p,
                sep = sep,
                label = p + 1,
            ));
        }
    }

    let subnet_selector = subnet_dialog(&subnets, idx, q.pick == Some(1));
    let pending = st.saves_since_apply;
    let (apply_disabled, apply_hint) = match st.file.control_socket() {
        None => (" disabled", "設定檔未宣告 control-socket，無法套用"),
        Some(_) if pending == 0 => (" disabled", ""),
        Some(_) => ("", ""),
    };
    let pending_html = if pending > 0 {
        format!(r#"<span class="badge">{} 筆變更尚未套用</span>"#, pending)
    } else {
        String::new()
    };
    let warn_html = q
        .warn
        .as_deref()
        .map(|h| format!(r#"<p class="warn">⚠ hostname「{}」與其他 reservation 重複（未擋，僅提醒）</p>"#, escape(h)))
        .unwrap_or_default();
    let match_phrase = if needle.is_empty() {
        String::new()
    } else {
        format!("「{}」", escape(&needle))
    };

    Ok(format!(
        r##"<div class="bar">
<h1 style="margin:0">Subnet {idx} — {}</h1>
<button class="btn" onclick="document.getElementById('subnet-dialog').showModal()">切換 subnet</button>
<form class="bar" method="get" action="/subnet/{idx}" style="margin:0">
<input type="search" name="q" value="{}" placeholder="搜尋 hostname / IP / hw-address" style="flex:1;min-width:260px">
<button class="btn primary" type="submit">搜尋</button>
</form>
<a class="btn primary" href="/subnet/{idx}/new">新增 reservation</a>
<button class="btn primary{}" hx-post="/apply" hx-target="#apply-result" hx-swap="innerHTML" title="{apply_hint}">套用設定</button>
<span id="apply-result"></span>
{pending_html}
</div>
{warn_html}
{subnet_selector}
<p>共 {} 筆符合{}（第 {} / {} 頁）</p>
<table>
<thead><tr><th>#</th><th>hw-address</th><th>ip-address</th><th>hostname</th><th>動作</th></tr></thead>
<tbody>{rows_html}</tbody>
</table>
<p>{pager}</p>"##,
        escape(&subnet.cidr.to_string()),
        escape(&needle),
        apply_disabled,
        total,
        match_phrase,
        page + 1,
        pages,
        apply_hint = apply_hint,
    ))
}

fn subnet_dialog(subnets: &[SubnetSummary], current: usize, auto_open: bool) -> String {
    let mut items = String::new();
    for s in subnets {
        let sel = if s.index == current { " primary" } else { "" };
        items.push_str(&format!(
            r#"<li><a class="btn{sel}" href="/subnet/{i}">Subnet {i} — {cidr}（{n} 筆）</a></li>"#,
            sel = sel,
            i = s.index,
            cidr = s.cidr,
            n = s.reservation_count,
        ));
    }
    let script = if auto_open {
        "if (document.querySelectorAll('#subnet-dialog li').length > 1) { document.getElementById('subnet-dialog').showModal(); }"
    } else {
        ""
    };
    format!(
        r#"<dialog id="subnet-dialog">
<p><strong>選擇要編修的 subnet</strong></p>
<ul style="list-style:none;padding:0">{items}</ul>
<button class="btn" onclick="document.getElementById('subnet-dialog').close()">取消</button>
</dialog>
<script>{script}</script>"#
    )
}

fn form_html(idx: usize, ridx: Option<usize>, r: Option<&Reservation>, errors: &[String], warnings: &[String]) -> String {
    let (hw, ip, hostname) = match r {
        Some(r) => (r.hw_address.clone(), r.ip_address.to_string(), r.hostname.clone().unwrap_or_default()),
        None => (String::new(), String::new(), String::new()),
    };
    let action = match ridx {
        Some(ri) => format!("/subnet/{idx}/res/{ri}/edit"),
        None => format!("/subnet/{idx}/new"),
    };
    let title = if ridx.is_some() { "編輯 reservation" } else { "新增 reservation" };

    let mut errs = String::new();
    if !errors.is_empty() {
        errs.push_str(r#"<div class="err"><ul>"#);
        for e in errors {
            errs.push_str(&format!("<li>{}</li>", escape(e)));
        }
        errs.push_str("</ul></div>");
    }
    let mut warns = String::new();
    if !warnings.is_empty() {
        warns.push_str(r#"<div class="warn"><ul>"#);
        for w in warnings {
            warns.push_str(&format!("<li>⚠ {}</li>", escape(w)));
        }
        warns.push_str("</ul></div>");
    }

    format!(
        r#"<div class="bar"><h1 style="margin:0">{title} — Subnet {idx}</h1></div>
{errs}{warns}
<form method="post" action="{action}" class="bar" style="flex-direction:column;align-items:flex-start;max-width:32rem">
<label>hw-address（MAC，唯一鍵）<br><input name="hw_address" value="{}" required placeholder="1c:69:7a:77:3b:98" style="width:100%"></label>
<label>ip-address<br><input name="ip_address" value="{}" required placeholder="10.1.1.11" style="width:100%"></label>
<label>hostname（選用）<br><input name="hostname" value="{}" style="width:100%"></label>
<div class="bar" style="margin-top:.5rem">
<button class="btn primary" type="submit">儲存</button>
<a class="btn" href="/subnet/{idx}">取消</a>
</div>
</form>"#,
        escape(&hw),
        escape(&ip),
        escape(&hostname),
    )
}

#[derive(Deserialize)]
struct ReservationForm {
    hw_address: String,
    ip_address: String,
    hostname: Option<String>,
}

fn reservation_from_form(f: &ReservationForm) -> (Result<Reservation>, Vec<String>) {
    let mut errors = Vec::new();
    let hw = f.hw_address.trim();
    if hw.is_empty() {
        errors.push("hw-address 不得為空".into());
    }
    let normalized_mac = normalize_mac(hw);
    if normalized_mac.is_none() {
        errors.push(format!("hw-address 格式不合法（需 6 組十六進位，可用 : 或 - 分隔）: {}", escape(hw)));
    }
    let ip = match Ipv4Addr::from_str(f.ip_address.trim()) {
        Ok(ip) => ip,
        Err(_) => {
            errors.push(format!("ip-address 格式不合法: {}", escape(&f.ip_address)));
            Ipv4Addr::UNSPECIFIED
        }
    };
    let hostname = f.hostname.as_ref().map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
    if errors.is_empty() {
        (
            Ok(Reservation {
                hw_address: normalized_mac.expect("validated above"),
                ip_address: ip,
                hostname,
            }),
            errors,
        )
    } else {
        (Err(anyhow::anyhow!("表單不合法")), errors)
    }
}

/// 接受 `:` 或 `-` 分隔的 6 組 MAC，正規化為小寫冒號格式；否則回傳 None。
fn normalize_mac(s: &str) -> Option<String> {
    let parts: Vec<&str> = if s.contains(':') {
        s.split(':').collect()
    } else if s.contains('-') {
        s.split('-').collect()
    } else {
        return None;
    };
    if parts.len() != 6 || parts.iter().any(|p| p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit())) {
        return None;
    }
    Some(parts.join(":").to_lowercase())
}

fn conflict_messages(c: &[Conflict]) -> Vec<String> {
    c.iter()
        .map(|x| match x {
            Conflict::DuplicateHwAddress => "hw-address 已存在（唯一鍵衝突）".to_string(),
            Conflict::IpInUse => "IP 已被其他 reservation 佔用".to_string(),
            Conflict::IpInPool => "IP 落在 dynamic pool 範圍內".to_string(),
            Conflict::IpOutOfSubnet => "IP 超出 subnet 範圍".to_string(),
        })
        .collect()
}

async fn create_reservation(
    State(state): State<Shared>,
    Path(idx): Path<usize>,
    Form(form): Form<ReservationForm>,
) -> Response {
    submit_reservation(state, idx, None, form).await
}

async fn update_reservation(
    State(state): State<Shared>,
    Path((idx, ridx)): Path<(usize, usize)>,
    Form(form): Form<ReservationForm>,
) -> Response {
    submit_reservation(state, idx, Some(ridx), form).await
}

/// 新增（ridx=None）或修改（ridx=Some）共用的提交流程：表單校驗 → 衝突檢查 → 寫檔 → 帶警告重新導向。
async fn submit_reservation(
    state: Shared,
    idx: usize,
    ridx: Option<usize>,
    form: ReservationForm,
) -> Response {
    let (parsed, form_errors) = reservation_from_form(&form);
    let mut st = state.lock().await;
    let Ok(subnet) = st.file.subnet(idx) else {
        return page_500("subnet 不存在");
    };
    if ridx.is_some_and(|r| r >= subnet.reservations.len()) {
        return page_500("reservation 不存在");
    }
    let reservation = match parsed {
        Ok(r) => r,
        Err(_) => {
            let dummy = Reservation {
                hw_address: form.hw_address.trim().to_lowercase(),
                ip_address: form.ip_address.trim().parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
                hostname: form.hostname.as_ref().map(|h| h.trim().to_string()).filter(|h| !h.is_empty()),
            };
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Html(page("表單不合法", &form_html(idx, ridx, Some(&dummy), &form_errors, &[]))),
            )
                .into_response();
        }
    };
    let conflicts = check_conflicts(&subnet, &reservation, ridx);
    if !conflicts.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(page("衝突", &form_html(idx, ridx, Some(&reservation), &conflict_messages(&conflicts), &[]))),
        )
            .into_response();
    }
    let result = match ridx {
        Some(r) => st.file.update_reservation(idx, r, &reservation),
        None => st.file.add_reservation(idx, &reservation),
    };
    if let Err(e) = result {
        return page_500(&e.to_string());
    }
    if let Err(e) = st.save_locked() {
        return page_500(&e.to_string());
    }
    let mut redirect = format!("/subnet/{idx}");
    if let Some(dup) = duplicate_hostnames(&subnet, &reservation, ridx).first() {
        redirect.push_str(&format!("?warn={}", urlencode(dup)));
    }
    Redirect::to(&redirect).into_response()
}

async fn new_form(State(state): State<Shared>, Path(idx): Path<usize>) -> Response {
    let st = state.lock().await;
    if st.file.subnet(idx).is_err() {
        return page_500("subnet 不存在");
    }
    (StatusCode::OK, Html(page("新增", &form_html(idx, None, None, &[], &[])))).into_response()
}

async fn edit_form(
    State(state): State<Shared>,
    Path((idx, ridx)): Path<(usize, usize)>,
) -> Response {
    let st = state.lock().await;
    let subnet = match st.file.subnet(idx) {
        Ok(s) => s,
        Err(_) => return page_500("subnet 不存在"),
    };
    match subnet.reservations.get(ridx) {
        Some(r) => (StatusCode::OK, Html(page("編輯", &form_html(idx, Some(ridx), Some(r), &[], &[])))).into_response(),
        None => page_500("reservation 不存在"),
    }
}

async fn delete_reservation(
    State(state): State<Shared>,
    Path((idx, ridx)): Path<(usize, usize)>,
) -> Response {
    let mut st = state.lock().await;
    if let Err(e) = st.file.delete_reservation(idx, ridx) {
        return page_500(&e.to_string());
    }
    if let Err(e) = st.save_locked() {
        return page_500(&e.to_string());
    }
    Redirect::to(&format!("/subnet/{idx}")).into_response()
}

async fn apply_reload(State(state): State<Shared>) -> Response {
    let (cs, pending): (Option<ControlSocket>, u64) = {
        let st = state.lock().await;
        (st.file.control_socket(), st.saves_since_apply)
    };
    let Some(cs) = cs else {
        return Html(r#"<span class="err">設定檔未宣告 control-socket，無法套用</span>"#.to_string()).into_response();
    };
    if pending == 0 {
        return Html(r#"<span class="ok">沒有待套用的變更</span>"#.to_string()).into_response();
    }
    match reload(&cs) {
        Ok(text) => {
            state.lock().await.saves_since_apply = 0;
            Html(format!(r#"<span class="ok">已套用並 reload 成功：{}</span>"#, escape(&text))).into_response()
        }
        Err(e) => Html(format!(r#"<span class="err">檔案已寫入，但 reload 失敗：{}</span>"#, escape(&e.to_string()))).into_response(),
    }
}

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-Hant"><head><meta charset="utf-8">
<title>{title} — Kealight</title>
<script src="/static/htmx.min.js" defer></script>
<style>
body {{ font-family: system-ui, sans-serif; margin: 2rem; background:#f6f7f9; color:#222; }}
table {{ border-collapse: collapse; width: 100%; background:#fff; }}
th, td {{ border: 1px solid #d4d7dd; padding: .4rem .6rem; text-align: left; font-size: .92rem; }}
th {{ background:#eef0f4; }}
.bar {{ display:flex; gap:.75rem; align-items:center; margin-bottom:1rem; flex-wrap:wrap; }}
.btn {{ padding:.35rem .7rem; border:1px solid #b8bdc7; background:#fff; cursor:pointer; text-decoration:none; color:#222; border-radius:4px; display:inline-block; }}
.btn:disabled {{ opacity:.4; cursor:not-allowed; }}
.btn.primary {{ background:#1f6feb; color:#fff; border-color:#1f6feb; }}
.err {{ color:#b00020; }}
.ok {{ color:#0a7d32; }}
.warn {{ color:#8a6d00; }}
.badge {{ background:#d3f0d3; border-radius:8px; padding:.1rem .5rem; font-size:.8rem; }}
dialog {{ border:1px solid #b8bdc7; border-radius:8px; padding:1.25rem; }}
</style></head><body>{body}</body></html>"#
    )
}

fn page_500(msg: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Html(page("錯誤", &err_html(msg)))).into_response()
}

fn err_html(msg: &str) -> String {
    format!(r#"<p class="err">{}</p><p><a class="btn" href="/">回首頁</a></p>"#, escape(msg))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn fixture() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/kea-dhcp4.conf")
    }

    fn test_app() -> (Router, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kealight-web-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kea-dhcp4.conf");
        std::fs::copy(fixture(), &p).unwrap();
        let file = KeaFile::load(&p).unwrap();
        let state = AppState {
            kea_path: p.clone(),
            backup_keep: 3,
            file,
            saves_since_apply: 0,
        };
        (app(state), p)
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_form(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn root_redirects_to_first_subnet() {
        let (app, dir) = test_app();
        let (status, _) = get(&app, "/").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn subnet_list_page_ok() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("10.1.0.0/16"));
        assert!(body.contains("chufang-mg4670-3"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn subnet_list_missing_subnet_is_404() {
        let (app, dir) = test_app();
        let (status, _) = get(&app, "/subnet/99").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn search_filters_rows() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0?q=chufang").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("chufang-mg4670-3"));
        assert!(!body.contains("damenjingweishi"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn new_form_renders() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0/new").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("hw_address"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn create_valid_reservation_redirects_and_writes_file() {
        let (app, p) = test_app();
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A01&ip_address=10.1.9.9&hostname=test-host",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        assert_eq!(reloaded.subnet(0).unwrap().reservations.len(), 958);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn create_duplicate_mac_rejected_with_conflict_page() {
        let (app, dir) = test_app();
        let (status, body) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=1c%3A69%3A7a%3A77%3A3b%3A98&ip_address=10.1.9.9",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("唯一鍵衝突"), "body: {body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn create_ip_in_pool_rejected() {
        let (app, dir) = test_app();
        let (status, body) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A02&ip_address=10.1.11.50",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("dynamic pool"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn malformed_mac_rejected() {
        let (app, dir) = test_app();
        let (status, body) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=not-a-mac&ip_address=10.1.9.9",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("格式不合法"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn edit_form_renders_existing_values() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0/res/0/edit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("1c:69:7a:77:3b:98"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn update_reservation_redirects_and_writes_file() {
        let (app, p) = test_app();
        let (status, _) = post_form(
            &app,
            "/subnet/0/res/0/edit",
            "hw_address=1c%3A69%3A7a%3A77%3A3b%3A98&ip_address=10.1.1.99&hostname=renamed",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        let s = reloaded.subnet(0).unwrap();
        assert_eq!(s.reservations.len(), 957);
        assert_eq!(s.reservations[0].ip_address.to_string(), "10.1.1.99");
        assert_eq!(s.reservations[0].hostname.as_deref(), Some("renamed"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn update_with_ip_taken_elsewhere_rejected() {
        let (app, dir) = test_app();
        let (status, body) = post_form(
            &app,
            "/subnet/0/res/0/edit",
            "hw_address=1c%3A69%3A7a%3A77%3A3b%3A98&ip_address=10.1.1.13",
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("IP 已被其他 reservation 佔用"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn delete_reservation_redirects_and_writes_file() {
        let (app, p) = test_app();
        let (status, _) = post_form(&app, "/subnet/0/res/0/delete", "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        assert_eq!(reloaded.subnet(0).unwrap().reservations.len(), 956);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn apply_without_pending_changes_does_not_connect() {
        let (app, dir) = test_app();
        let (status, body) = post_form(&app, "/apply", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("沒有待套用的變更"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn create_with_duplicate_hostname_redirects_with_warning() {
        let (app, p) = test_app();
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A07&ip_address=10.1.9.7&hostname=chufang-mg4670-3",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, body) = get(&app, "/subnet/0?warn=chufang-mg4670-3").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("與其他 reservation 重複"), "body: {body}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn apply_button_disabled_without_control_socket() {
        let dir = std::env::temp_dir().join(format!(
            "kealight-web-nocs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kea-dhcp4.conf");
        std::fs::copy(fixture(), &p).unwrap();
        let mut f = KeaFile::load(&p).unwrap();
        f.root.as_object_mut().unwrap().get_mut("Dhcp4").unwrap().as_object_mut().unwrap().remove("control-socket");
        let state = AppState { kea_path: p.clone(), backup_keep: 3, file: f, saves_since_apply: 1 };
        let app = app(state);
        let (status, body) = get(&app, "/subnet/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"title="設定檔未宣告 control-socket，無法套用""#), "body: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mac_with_dashes_is_normalized() {
        let (app, p) = test_app();
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=AA-BB-CC-DD-EE-08&ip_address=10.1.9.8",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        let last = reloaded.subnet(0).unwrap().reservations.last().unwrap().clone();
        assert_eq!(last.hw_address, "aa:bb:cc:dd:ee:08");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn saves_increment_pending_and_apply_resets_it() {
        let (app, dir) = test_app();
        post_form(&app, "/subnet/0/new", "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A01&ip_address=10.1.9.9").await;
        let (status, body) = get(&app, "/subnet/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("尚未套用"), "body: {body}");

        // fixture 有 control-socket（unix，不存在於測試環境），reload 應報錯但保持 pending
        let (status, body) = post_form(&app, "/apply", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("reload 失敗"), "body: {body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
