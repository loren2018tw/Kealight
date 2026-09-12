use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{bail, Result};
use axum::extract::{Form, Path, Query, State};
use axum::extract::connect_info::ConnectInfo;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::domain::{check_conflicts, duplicate_hostnames, Conflict, Reservation};
use crate::file::{backup, file_stat, FileStat, KeaFile, LeaseDb, SubnetSummary};
use crate::leases::{matches_state_filter, parse as parse_leases, remaining_text, row_state_label, Lease};
use crate::reload::reload;

const PAGE_SIZE: usize = 50;
const HTMX_JS: &str = include_str!("../static/htmx.min.js");

/// 一次請求的「kea 設定檔外部修改」檢查結果。
pub enum Refresh {
    /// 與基準一致（含首次請求建立基準）
    Unchanged,
    /// 已靜默重載為外部版本（當下無 pending）
    Reloaded,
    /// 外部版本無法解析，保留記憶體版本
    ParseError,
    /// 設定檔不存在
    MissingFile,
    /// 有 pending 且偵測到外部修改：未重載，需橫幅決策
    ExternalChanged,
}

pub struct AppState {
    pub kea_path: PathBuf,
    pub backup_keep: usize,
    pub file: KeaFile,
    pub saves_since_apply: u64,
    /// 上次載入／寫回時的檔案 stat 基準（mtime 奈秒＋大小）
    pub baseline_stat: Option<FileStat>,
}

impl AppState {
    /// 載入 kea 設定檔並在**載入當下**建立 stat 基準——
    /// 否則「啟動後、首次請求前」的外部修改會因基準為空而漏判（ADR 0003）。
    pub fn load(kea_path: PathBuf, backup_keep: usize) -> Result<AppState> {
        let file = KeaFile::load(&kea_path)?;
        let baseline_stat = match file_stat(&kea_path) {
            Ok(s) => s,
            Err(_) => None,
        };
        Ok(AppState {
            kea_path,
            backup_keep,
            file,
            saves_since_apply: 0,
            baseline_stat,
        })
    }

    fn save_locked(&mut self) -> Result<()> {
        backup(&self.kea_path, self.backup_keep)?;
        self.file.save(&self.kea_path)?;
        self.saves_since_apply += 1;
        let Ok(st) = file_stat(&self.kea_path) else {
            return Ok(());
        };
        self.baseline_stat = st;
        Ok(())
    }

    /// 每請求檢查外部修改（ADR 0003）：stat 比對 → 需要時重載。
    /// 自家寫回後基準已更新，不會把自己誤判為外部修改。
    fn refresh_external(&mut self) -> Refresh {
        let Ok(some_stat) = file_stat(&self.kea_path) else {
            return Refresh::MissingFile;
        };
        let Some(cur) = some_stat else {
            return Refresh::MissingFile;
        };
        match self.baseline_stat.as_ref() {
            None => {
                self.baseline_stat = Some(cur);
                Refresh::Unchanged
            }
            Some(b) if *b == cur => Refresh::Unchanged,
            Some(_) => {
                if self.saves_since_apply > 0 {
                    Refresh::ExternalChanged
                } else {
                    let Ok(f) = KeaFile::load(&self.kea_path) else {
                        return Refresh::ParseError;
                    };
                    self.file = f;
                    self.baseline_stat = Some(cur);
                    Refresh::Reloaded
                }
            }
        }
    }

    /// 「捨棄變更重新載入」：強制重載外部版本，pending 歸零。
    fn adopt_external(&mut self) -> Result<()> {
        let Ok(f) = KeaFile::load(&self.kea_path) else {
            bail!("設定檔不存在或無法解析，無法重新載入外部版本");
        };
        self.file = f;
        self.saves_since_apply = 0;
        let Ok(st) = file_stat(&self.kea_path) else {
            return Ok(());
        };
        self.baseline_stat = st;
        Ok(())
    }

    /// 「以本系統覆寫」：把記憶體版本寫回磁碟（不計入 pending），並更新基準。
    fn overwrite_external(&mut self) -> Result<()> {
        backup(&self.kea_path, self.backup_keep)?;
        self.file.save(&self.kea_path)?;
        let Ok(st) = file_stat(&self.kea_path) else {
            return Ok(());
        };
        self.baseline_stat = st;
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
        .route("/leases", get(leases_page))
        .route("/external/adopt", post(external_adopt))
        .route("/external/overwrite", post(external_overwrite))
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
    sort: Option<String>,
    dir: Option<String>,
}

async fn subnet_list(
    State(state): State<Shared>,
    Path(idx): Path<usize>,
    Query(q): Query<ListQuery>,
) -> Response {
    let (html, status) = {
        let mut st = state.lock().await;
        let refresh = st.refresh_external();
        match render_subnet_list(&st, idx, &q, &refresh) {
            Ok(body) => (page(&format!("Subnet {idx}"), &body), StatusCode::OK),
            Err(e) => (page("錯誤", &err_html(&e.to_string())), StatusCode::NOT_FOUND),
        }
    };
    (status, Html(html)).into_response()
}

fn render_subnet_list(st: &AppState, idx: usize, q: &ListQuery, refresh: &Refresh) -> Result<String> {
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
    let mut filtered: Vec<(usize, &Reservation)> = subnet
        .reservations
        .iter()
        .enumerate()
        .filter(|(_, r)| matches(r))
        .collect();
    let sort_field = q
        .sort
        .as_deref()
        .map(|s| s.to_string())
        .filter(|s| s == "hw-address" || s == "ip-address" || s == "hostname");
    if let Some(sf) = q.sort.clone()
        && (sf == "hw-address" || sf == "ip-address" || sf == "hostname")
    {
        let desc = q.dir.as_deref().map(|d| *d == *"desc").unwrap_or(false);
        filtered.sort_by_key(|(orig, r)| sort_key_bytes(r, &sf, desc, *orig));
    }
    let total = filtered.len();
    let searching = !needle.is_empty();
    let (pages, page, start, end) = page_window(total, q.page, searching);
    let rows = &filtered[start..end];

    let mut rows_html = String::new();
    for (orig, r) in rows.iter() {
        let global = *orig;
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
        format!(
            r#"<span id="pending-badge" class="badge">{} 筆變更尚未套用</span>"#,
            pending
        )
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
    let page_phrase = if searching {
        String::new()
    } else {
        format!("（第 {} / {} 頁）", page + 1, pages)
    };
    let mut th_html = String::new();
    th_html.push_str(&th_link(
        "hw-address",
        "hw-address",
        idx,
        &sort_field,
        q.dir.as_deref(),
    ));
    th_html.push_str(&th_link(
        "ip-address",
        "ip-address",
        idx,
        &sort_field,
        q.dir.as_deref(),
    ));
    th_html.push_str(&th_link(
        "hostname",
        "hostname",
        idx,
        &sort_field,
        q.dir.as_deref(),
    ));
    let sort_vals = sort_hx_vals(sort_field.clone(), q.dir.as_deref().map(|x| *x == *"desc").unwrap_or(false));

    let mut out = String::new();
    out.push_str(&tab_strip("hosts", &format!("/subnet/{idx}")));
    let next = subnet_page_url(idx, q);
    out.push_str(&external_banner_html(refresh, st.saves_since_apply, &next));
    out.push_str(&format!(
        r##"<div class="bar">
<h1 style="margin:0">Subnet {idx} — {}</h1>
<button class="btn" onclick="document.getElementById('subnet-dialog').showModal()">切換 subnet</button>
<input type="search" name="q" value="{}" placeholder="搜尋 hostname / IP / hw-address（即時篩選）" style="flex:1;min-width:260px"
hx-get="/subnet/{idx}" hx-trigger="input changed delay:200ms" hx-target="#list-panel" hx-select="#list-panel" hx-swap="outerHTML"{sort_vals}>
<a class="btn primary" href="/subnet/{idx}/new">新增 reservation</a>
<button class="btn primary{}" hx-post="/apply" hx-target="#apply-result" hx-swap="innerHTML" title="{apply_hint}">套用設定</button>
<span id="apply-result"></span>
{pending_html}
</div>
{warn_html}
{subnet_selector}
<div id="list-panel">
<p>共 {} 筆符合{}{}</p>
<table>
<thead><tr><th>#</th>{th_html}<th>動作</th></tr></thead>
<tbody>{rows_html}</tbody>
</table>
<p>{pager}</p>
</div>"##,
        escape(&subnet.cidr.to_string()),
        escape(&needle),
        apply_disabled,
        total,
        match_phrase,
        page_phrase,
        apply_hint = apply_hint,
    ));
    Ok(out)
}

/// 首頁分頁列。active: "hosts" | "leases"；hosts_href 為「保留主機」分頁連結。
fn tab_strip(active: &str, hosts_href: &str) -> String {
    let hosts = if active == "hosts" { " primary" } else { "" };
    let leases = if active == "leases" { " primary" } else { "" };
    format!(
        r##"<nav class="tabs">
<a class="btn{hosts}" href="{hosts_href}">保留主機</a>
<a class="btn{leases}" href="/leases">作用中租用</a>
</nav>"##,
        hosts = hosts,
        hosts_href = hosts_href,
        leases = leases,
    )
}

/// GET 頁面的外部修改橫幅（依 refresh 結果）；不需要橫幅時回傳空字串。
fn external_banner_html(refresh: &Refresh, pending: u64, next: &str) -> String {
    match refresh {
        Refresh::MissingFile => r#"<p class="err"><strong>設定檔已被刪除。</strong>介面仍顯示上次載入的版本；編輯寫回會重建設定檔。</p>"#
            .to_string(),
        Refresh::ParseError => r#"<p class="err"><strong>設定檔已被外部修改且無法解析。</strong>介面仍顯示上次載入的版本。</p>"#
            .to_string(),
        Refresh::ExternalChanged => format!(
            r##"<p class="warn"><strong>設定檔已被外部修改</strong>（本系統尚有 {pending} 筆變更未套用）。先決定以哪一方為準：</p>
<form method="post" action="/external/adopt" class="inline-form">
<input type="hidden" name="next" value="{next}">
<button class="btn" title="捨棄本系統未套用的變更，載入外部版本">捨棄變更重新載入</button>
</form>
<form method="post" action="/external/overwrite" class="inline-form">
<input type="hidden" name="next" value="{next}">
<button class="btn primary" title="以本系統版本覆寫設定檔（未套用變更保留）">以本系統覆寫</button>
</form>"##,
            pending = pending,
            next = escape(next),
        )
            .to_string(),
        _ => String::new(),
    }
}

/// POST 攔截頁：設定檔已被外部修改的決策頁。
/// hidden 為 Some 時（create/update）以單表單＋name=external_resolve 按鈕重送原意圖；
/// 為 None 時（delete/apply）以雙表單把 external_resolve 走 query string。
fn external_decision_page(
    action: &str,
    hidden: Option<&str>,
    summary: &str,
    back: &str,
    pending: u64,
) -> Response {
    let body = match hidden {
        Some(h) => format!(
            r##"<div class="bar"><h1 style="margin:0">設定檔已被外部修改</h1></div>
<p class="warn">本系統尚有 {pending} 筆變更未套用，但設定檔已被外部修改。先決定以哪一方為準，再繼續本次操作：</p>
<p>{summary}</p>
<form method="post" action="{action}">
{hidden}
<div class="bar">
<button type="submit" name="external_resolve" value="adopt" class="btn" title="捨棄本系統未套用的變更，以外部版本為基底繼續本次操作">捨棄變更重新載入</button>
<button type="submit" name="external_resolve" value="overwrite" class="btn primary" title="以本系統版本覆寫設定檔，再繼續本次操作">以本系統覆寫</button>
</div>
</form>
<p><a class="btn" href="{back}">取消，先回列表</a></p>"##,
            pending = pending,
            summary = summary,
            action = action,
            hidden = h,
            back = back,
        ),
        None => format!(
            r##"<div class="bar"><h1 style="margin:0">設定檔已被外部修改</h1></div>
<p class="warn">本系統尚有 {pending} 筆變更未套用，但設定檔已被外部修改。先決定以哪一方為準，再繼續本次操作：</p>
<p>{summary}</p>
<form method="post" action="{action}?external_resolve=adopt" class="inline-form">
<button type="submit" class="btn" title="捨棄本系統未套用的變更，以外部版本為基底繼續本次操作">捨棄變更重新載入</button>
</form>
<form method="post" action="{action}?external_resolve=overwrite" class="inline-form">
<button type="submit" class="btn primary" title="以本系統版本覆寫設定檔，再繼續本次操作">以本系統覆寫</button>
</form>
<p><a class="btn" href="{back}">取消，先回列表</a></p>"##,
            pending = pending,
            summary = summary,
            action = action,
            back = back,
        ),
    };
    (StatusCode::CONFLICT, Html(page("外部修改", &body))).into_response()
}

/// 外部修改攔截的決策處理：「adopt」採用外部版並歸零 pending；「overwrite」以本系統覆寫。
/// 回 Some 表示請求應以此回應中止；None 表示決策已套用、呼叫者繼續原意圖。
fn resolve_external(
    st: &mut AppState,
    decision: Option<&str>,
    intercept: Response,
) -> Option<Response> {
    match decision {
        Some(d) if *d == *"adopt" => {
            if let Err(e) = st.adopt_external() {
                Some(page_500(&e.to_string()))
            } else {
                None
            }
        }
        Some(d) if *d == *"overwrite" => {
            if let Err(e) = st.overwrite_external() {
                Some(page_500(&e.to_string()))
            } else {
                None
            }
        }
        _ => Some(intercept),
    }
}

/// 分頁視窗：回傳 (pages, current, start, end)。搜尋中一律視為單頁。
fn page_window(total: usize, page: Option<usize>, searching: bool) -> (usize, usize, usize, usize) {
    let pages = if searching { 1 } else { total.div_ceil(PAGE_SIZE) };
    let cur = if searching { 0 } else { page.unwrap_or(0).min(pages.saturating_sub(1)) };
    let start = if searching { 0 } else { cur * PAGE_SIZE };
    let end = if searching { total } else { total.min(start + PAGE_SIZE) };
    (pages, cur, start, end)
}

/// htmx 排序參數（hx-vals），保留目前排序方向。
fn sort_hx_vals(sort_field: Option<String>, desc: bool) -> String {
    match sort_field {
        Some(sf) => format!(
            r#" hx-vals='{{"sort":"{sf}","dir":"{d}"}}'"#,
            sf = sf,
            d = if desc { "desc" } else { "asc" },
        ),
        None => String::new(),
    }
}

/// 目前列表參數對應的 subnet 頁網址（外部修改橫幅決策後的返回點；值已 urlencode）。
fn subnet_page_url(idx: usize, q: &ListQuery) -> String {
    let mut params = Vec::new();
    if let Some(needle) = q.q.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("q={}", urlencode(needle)));
    }
    if let Some(p) = q.page.as_ref() {
        if *p != 0 {
            params.push(format!("page={p}"));
        }
    }
    if let Some(so) = q.sort.as_ref() {
        params.push(format!("sort={}", urlencode(so)));
        if let Some(d) = q.dir.as_ref() {
            params.push(format!("dir={}", urlencode(d)));
        }
    }
    if params.is_empty() {
        format!("/subnet/{idx}")
    } else {
        format!("/subnet/{idx}?{}", params.join("&"))
    }
}

#[derive(Deserialize, Default)]
struct NextQuery {
    next: Option<String>,
}

fn redirect_to_next(next: &Option<String>) -> Redirect {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => Redirect::to(n),
        _ => Redirect::to("/subnet/0"),
    }
}

async fn external_adopt(State(state): State<Shared>, Query(q): Query<NextQuery>) -> Response {
    let mut st = state.lock().await;
    if let Err(e) = st.adopt_external() {
        return page_500(&e.to_string());
    }
    redirect_to_next(&q.next).into_response()
}

async fn external_overwrite(State(state): State<Shared>, Query(q): Query<NextQuery>) -> Response {
    let mut st = state.lock().await;
    if let Err(e) = st.overwrite_external() {
        return page_500(&e.to_string());
    }
    redirect_to_next(&q.next).into_response()
}

fn subnet_dialog(subnets: &[SubnetSummary], current: usize, auto_open: bool) -> String {
    let mut items = String::new();
    for s in subnets.iter() {
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

#[derive(Deserialize, Default)]
struct LeasesQuery {
    q: Option<String>,
    page: Option<usize>,
    state: Option<String>,
    subnet: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
}

async fn leases_page(State(state): State<Shared>, Query(q): Query<LeasesQuery>) -> Response {
    let (html, status) = {
        let mut st = state.lock().await;
        let refresh = st.refresh_external();
        match render_leases(&st, &q, &refresh) {
            Ok(body) => (page("作用中租用", &body), StatusCode::OK),
            Err(e) => (page("錯誤", &err_html(&e.to_string())), StatusCode::INTERNAL_SERVER_ERROR),
        }
    };
    (status, Html(html)).into_response()
}

fn render_leases(st: &AppState, q: &LeasesQuery, refresh: &Refresh) -> Result<String> {
    let mut leases: Vec<Lease> = Vec::new();
    let mut empty_state = String::new();
    match st.file.lease_db() {
        LeaseDb::Unavailable(msg) => {
            empty_state = format!(r#"<p class="warn">{}</p>"#, escape(&msg));
        }
        LeaseDb::Memfile(path) => {
            match std::fs::read_to_string(&path) {
                Ok(text) => match parse_leases(&text) {
                    Ok(parsed) => leases = parsed,
                    Err(e) => {
                        empty_state = format!(
                            r#"<p class="warn">租用檔無法解析：{}</p>"#,
                            escape(&e.to_string())
                        );
                    }
                },
                Err(_) => {
                    let ptext = format!("{}", path.display());
                    empty_state = format!(
                        r#"<p class="warn">租用檔不存在：{}</p>"#,
                        escape(&ptext)
                    );
                }
            }
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("取得時間失敗: {e}"))?
        .as_secs() as u64;
    let cur_state = q.state.as_deref().map(|s| s.to_string()).unwrap_or_default();
    let subnet_sel: Option<u32> = q
        .subnet
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok());
    let matches_subnet = |l: &Lease| -> bool {
        match subnet_sel {
            None => true,
            Some(sid) => l.subnet_id == Some(sid),
        }
    };
    let needle = q.q.clone().unwrap_or_default().trim().to_lowercase();
    let matches = |l: &Lease| -> bool {
        if needle.is_empty() {
            return true;
        }
        l.address.to_string().contains(&needle)
            || l.hwaddr.as_deref().map(|h| h.to_lowercase().contains(&needle)).unwrap_or(false)
            || l.client_id
                .as_deref()
                .map(|h| h.to_lowercase().contains(&needle))
                .unwrap_or(false)
            || l.hostname
                .as_deref()
                .map(|h| h.to_lowercase().contains(&needle))
                .unwrap_or(false)
    };

    let mut filtered: Vec<(usize, &Lease)> = leases
        .iter()
        .enumerate()
        .filter(|(_, l)| matches_state_filter(l, &cur_state, now) && matches_subnet(l) && matches(l))
        .collect();
    let sort_field = q
        .sort
        .clone()
        .filter(|s| s == "address" || s == "hwaddr" || s == "hostname");
    let desc = q.dir.as_deref().map(|d| *d == *"desc").unwrap_or(false);
    if let Some(sf) = sort_field.clone() {
        filtered.sort_by_key(|(orig, l)| lease_sort_key(l, &sf, desc, *orig));
    }
    let total = filtered.len();
    let searching = !needle.is_empty();
    let (pages, page, start, end) = page_window(total, q.page, searching);
    let rows = &filtered[start..end];

    let subnets = st.file.subnet_list();
    let hwaddrs = st.file.reservation_hwaddrs();
    let mut rows_html = String::new();
    if rows.is_empty() {
        rows_html.push_str(r#"<tr><td colspan="7"><p>沒有符合的租用</p></td></tr>"#);
    }
    for (_, l) in rows.iter() {
        let reserved = l
            .hwaddr
            .as_deref()
            .map(|h| hwaddrs.contains(&h.to_lowercase()))
            .unwrap_or(false);
        let badge = if reserved {
            r#"<span class="badge">保留</span>"#
        } else {
            "".into()
        };
        let id_text = match l.hwaddr.as_deref() {
            Some(h) => escape(&h.to_lowercase()),
            None => match l.client_id.as_deref() {
                Some(c) => escape(c),
                None => r#"<span class="dim">—</span>"#.to_string(),
            },
        };
        let host_text = match l.hostname.as_deref() {
            Some(h) => escape(&h.to_string()),
            None => "".into(),
        };
        let subnet_text = match l.subnet_id {
            Some(sid) => {
                let mut text = "?".into();
                for s in subnets.iter() {
                    if s.id == Some(sid) {
                        text = s.cidr.clone();
                        break;
                    }
                }
                text
            }
            None => "?".into(),
        };
        let remain = remaining_text(l.expire, now).unwrap_or("已過期".into());
        let remain_cell = format!(
            r##"<td title="租用期 {} 秒">{}</td>"##,
            l.valid_lifetime,
            remain,
        );
        rows_html.push_str(&format!(
            r#"<tr>
<td>{addr}</td><td>{id}</td><td>{host}</td><td>{subnet}</td><td>{state}</td>{remain_cell}<td>{badge}</td>
</tr>"#,
            addr = escape(&l.address.to_string()),
            id = id_text,
            host = host_text,
            subnet = escape(&subnet_text),
            state = row_state_label(l, now),
            remain_cell = remain_cell,
            badge = badge,
        ));
    }

    let mut th_html = String::new();
    th_html.push_str(&leases_th_link("address", "address", &q));
    th_html.push_str(&leases_th_link("hwaddr", "hwaddr", &q));
    th_html.push_str(&leases_th_link("hostname", "hostname", &q));
    let sort_vals = sort_hx_vals(sort_field.clone(), desc);
    let sel_vals = sort_vals.clone();
    let state_opts = |v: &str, label: &str| -> String {
        let sel = if v.to_string() == cur_state { " selected" } else { "" };
        format!(r#"<option value="{v}"{sel}>{label}</option>"#, sel = sel)
    };
    let mut state_select = String::new();
    state_select.push_str(&state_opts("active", "作用中"));
    state_select.push_str(&state_opts("all", "全部"));
    state_select.push_str(&state_opts("expired", "已過期"));
    state_select.push_str(&state_opts("declined", "拒絕"));
    state_select.push_str(&state_opts("released", "已釋放"));
    let all_sel = if subnet_sel.is_none() { " selected" } else { "" };
    let mut subnet_opts = String::new();
    subnet_opts.push_str(&format!(
        r#"<option value=""{all_sel}>全部 subnet</option>"#,
        all_sel = all_sel
    ));
    for s in subnets.iter() {
        if let Some(sid) = s.id {
            let sel = if subnet_sel == Some(sid) { " selected" } else { "" };
            subnet_opts.push_str(&format!(
                r#"<option value="{sid}"{sel}>{cidr}（{n} 筆 reservation）</option>"#,
                sel = sel,
                cidr = s.cidr,
                n = s.reservation_count,
            ));
        }
    }

    let mut pager = String::new();
    if pages > 1 {
        for p in 0..pages {
            let url = leases_page_url(&q, Some(p));
            let cls = if p == page { " primary" } else { "" };
            pager.push_str(&format!(
                r##"<a class="btn{cls}" href="{url}" hx-get="{url}" hx-target="#lease-panel" hx-select="#lease-panel" hx-swap="outerHTML">{label}</a> "##,
                cls = cls,
                url = escape(&url),
                label = p + 1,
            ));
        }
    }

    let match_phrase = if needle.is_empty() {
        String::new()
    } else {
        format!("「{}」", escape(&needle))
    };
    let page_phrase = if searching {
        String::new()
    } else {
        format!("（第 {} / {} 頁）", page + 1, pages)
    };
    let refresh_url = leases_page_url(&q, None);
    let mut out = String::new();
    out.push_str(&tab_strip("leases", "/subnet/0"));
    let next = leases_page_url(&q, None);
    out.push_str(&external_banner_html(refresh, st.saves_since_apply, &next));
    out.push_str(&format!(
        r##"<div class="bar">
<h1 style="margin:0">作用中租用</h1>
<a class="btn" href="{refresh_url}">重新整理</a>
<select name="state"{sel_vals} hx-get="/leases" hx-trigger="change" hx-target="#lease-panel" hx-select="#lease-panel" hx-swap="outerHTML" hx-include="[name='q'],[name='subnet']" title="狀態篩選">
{state_select}
</select>
<select name="subnet"{sel_vals} hx-get="/leases" hx-trigger="change" hx-target="#lease-panel" hx-select="#lease-panel" hx-swap="outerHTML" hx-include="[name='q'],[name='state']" title="subnet 篩選">
{subnet_opts}
</select>
<input type="search" name="q" value="{}" placeholder="搜尋 address / hwaddr / hostname（即時篩選）" style="flex:1;min-width:260px"
hx-get="/leases" hx-trigger="input changed delay:200ms" hx-target="#lease-panel" hx-select="#lease-panel" hx-swap="outerHTML" hx-include="[name='state'],[name='subnet']"{sort_vals}>
</div>
{empty_state}
<div id="lease-panel">
<p>共 {} 筆符合{}{}</p>
<table>
<thead><tr>{th_html}<th>subnet</th><th>state</th><th>到期</th><th>保留</th></tr></thead>
<tbody>{rows_html}</tbody>
</table>
<p>{pager}</p>
</div>"##,
        escape(&needle),
        total,
        match_phrase,
        page_phrase,
        refresh_url = escape(&refresh_url),
        sel_vals = sel_vals,
        state_select = state_select,
        subnet_opts = subnet_opts,
        sort_vals = sort_vals,
        empty_state = empty_state,
    ));
    Ok(out)
}

/// 租用排序鍵：address 以數值（4 位元組大序）排列，其餘規則與 reservation 相同。
fn lease_sort_key(l: &Lease, sort: &str, desc: bool, orig: usize) -> Vec<u8> {
    let (present, payload) = match sort {
        "address" => {
            let v = u32::from(l.address);
            (
                true,
                vec![(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8],
            )
        }
        "hwaddr" => (
            l.hwaddr.is_some(),
            l.hwaddr
                .as_deref()
                .map(|h| h.to_lowercase().bytes().collect())
                .unwrap_or_default(),
        ),
        _ => (
            l.hostname.is_some(),
            l.hostname
                .as_deref()
                .map(|h| h.to_lowercase().bytes().collect())
                .unwrap_or_default(),
        ),
    };
    let mut key: Vec<u8> = Vec::new();
    key.push(if present { 0 } else { 1 });
    for b in payload {
        key.push(if desc { 0xFF - b } else { b });
    }
    key.push((orig >> 24) as u8);
    key.push((orig >> 16) as u8);
    key.push((orig >> 8) as u8);
    key.push(orig as u8);
    key
}

/// 租用頁排序欄位連結：三態（asc → desc → 無排序），保留其他篩選參數。
fn leases_th_link(field: &str, label: &str, q: &LeasesQuery) -> String {
    let is_current = q.sort.as_deref().map(|s| *s == *field).unwrap_or(false);
    let desc = q.dir.as_deref().map(|d| *d == *"desc").unwrap_or(false);
    let marker = if is_current {
        if desc {
            " ▼"
        } else {
            " ▲"
        }
    } else {
        ""
    };
    let base = leases_base_href(&q);
    let href = if !is_current {
        format!("{base}sort={field}&dir=asc")
    } else if desc {
        "/leases".into()
    } else {
        format!("{base}sort={field}&dir=desc")
    };
    format!(
        r##"<th><a href="{href}" hx-get="{href}" hx-target="#lease-panel" hx-select="#lease-panel" hx-swap="outerHTML">{label}{marker}</a></th>"##,
        href = escape(&href),
        label = label,
        marker = marker,
    )
}

/// `/leases？` 開頭，含 q/state/subnet（不含 sort/dir/page），尾接「&」或空。
fn leases_base_href(q: &LeasesQuery) -> String {
    let mut params = Vec::new();
    if let Some(v) = q.q.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("q={}", urlencode(v)));
    }
    if let Some(v) = q.state.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("state={}", urlencode(v)));
    }
    if let Some(v) = q.subnet.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("subnet={v}"));
    }
    if params.is_empty() {
        "/leases?".into()
    } else {
        format!("/leases?{}&", params.join("&"))
    }
}

/// 完整 /leases URL（含目前 sort/dir/q/state/subnet；page 由參數覆蓋）。
fn leases_page_url(q: &LeasesQuery, page: Option<usize>) -> String {
    let mut params = Vec::new();
    if let Some(v) = q.q.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("q={}", urlencode(v)));
    }
    if let Some(v) = q.state.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("state={}", urlencode(v)));
    }
    if let Some(v) = q.subnet.as_ref().filter(|s| !s.is_empty()) {
        params.push(format!("subnet={v}"));
    }
    if let Some(p) = page {
        if p != 0 {
            params.push(format!("page={p}"));
        }
    } else if let Some(v) = q.page.as_ref() {
        if *v != 0 {
            params.push(format!("page={v}"));
        }
    }
    if let Some(v) = q.sort.as_ref() {
        params.push(format!("sort={}", urlencode(v)));
        if let Some(d) = q.dir.as_ref() {
            params.push(format!("dir={}", urlencode(d)));
        }
    }
    if params.is_empty() {
        "/leases".into()
    } else {
        format!("/leases?{}", params.join("&"))
    }
}

fn form_html(
    idx: usize,
    ridx: Option<usize>,
    r: Option<&Reservation>,
    errors: &[String],
    warnings: &[String],
    peer_mac: Option<String>,
) -> String {
    let (hw, ip, hostname) = match r {
        Some(r) => (
            r.hw_address.clone(),
            r.ip_address.to_string(),
            r.hostname.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let peer_hint = match peer_mac {
        Some(m) => format!(
            r##"<span class="hint">目前連線主機 MAC：{m}（可供手動填入）</span>"##,
            m = escape(m.as_str()),
        ),
        None => r##"<span class="hint">無法取得連線主機 MAC（需與本系統同一層網路）</span>"##
            .to_string(),
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
<label>hw-address（MAC，唯一鍵）<br><input name="hw_address" value="{}" required placeholder="1c:69:7a:77:3b:98" style="width:100%"><br>{peer_hint}</label>
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
    /// 外部修改決策頁重送時攜帶：adopt | overwrite
    external_resolve: Option<String>,
}

#[derive(Deserialize, Default)]
struct ExternalResolveQuery {
    external_resolve: Option<String>,
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

/// 排序鍵：v1 全序唯一，不依賴 sort_by_key 的穩定性。
/// 前綴 0=有值 / 1=無值（無 hostname 者恆排尾，升降冪皆然）；
/// desc 時 payload 逐 byte 補數（反向排序但不動 None 尾）；尾綴原始索引做穩定 tie-breaker。
fn sort_key_bytes(r: &Reservation, sort: &str, desc: bool, orig: usize) -> Vec<u8> {
    let (present, payload) = match sort {
        "ip-address" => (
            true,
            r.ip_address
                .to_string()
                .split('.')
                .map(|p| format!("{:0>3}", p))
                .collect::<Vec<String>>()
                .join("."),
        ),
        "hw-address" => (true, r.hw_address.to_lowercase()),
        _ => (
            r.hostname.is_some(),
            r.hostname
                .as_deref()
                .map(|h| h.to_lowercase())
                .unwrap_or_default(),
        ),
    };
    let mut key: Vec<u8> = Vec::new();
    key.push(if present { 0 } else { 1 });
    let payload_bytes: Vec<u8> = payload.bytes().collect();
    for b in payload_bytes {
        key.push(if desc { 0xFF - b } else { b });
    }
    key.push((orig >> 24) as u8);
    key.push((orig >> 16) as u8);
    key.push((orig >> 8) as u8);
    key.push(orig as u8);
    key
}

/// 欄名排序連結：三態（asc → desc → 無排序），以 ▲/▼ 標示目前方向；
/// 以 htmx 只替換 #list-panel 並保留搜尋詞（hx-include 輸入框 q）。
fn th_link(
    field: &str,
    label: &str,
    idx: usize,
    cur_sort: &Option<String>,
    cur_dir: Option<&str>,
) -> String {
    let is_current = cur_sort.as_deref().map(|s| *s == *field).unwrap_or(false);
    let desc = cur_dir.map(|d| *d == *"desc").unwrap_or(false);
    let marker = if is_current {
        if desc { " ▼" } else { " ▲" }
    } else {
        ""
    };
    let href = if !is_current {
        format!("/subnet/{idx}?sort={field}&dir=asc")
    } else if desc {
        format!("/subnet/{idx}")
    } else {
        format!("/subnet/{idx}?sort={field}&dir=desc")
    };
    format!(
        r##"<th><a href="{href}" hx-get="{href}" hx-include="[name='q']" hx-target="#list-panel" hx-select="#list-panel" hx-swap="outerHTML">{label}{marker}</a></th>"##,
        href = href,
        label = label,
        marker = marker,
    )
}

/// 解析 `/proc/net/arp` 文字，反查 target_ip 對應的 MAC。
/// 標題列、incomplete（00:00:00:00:00:00）或無命中皆回傳 None。
fn mac_from_arp_table(table: &str, target_ip: &str) -> Option<String> {
    for line in table.split('\n') {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 4 || words[0] == "IP" {
            continue;
        }
        if words[0] == target_ip && words[3] != "00:00:00:00:00:00" {
            return Some(words[3].to_string());
        }
    }
    None
}

/// 讀取本機 ARP 表，反查操作主機（連線來源）的 MAC。
/// 瀏覽器拿不到本機 MAC，但操作主機與本系統在同一層網路（同廣播域）時，
/// 伺服器可從自身 ARP 表反查連線來源 IP。跨網段、VPN 或 bind localhost 時查不到。
fn peer_mac(peer: &SocketAddr) -> Option<String> {
    match peer.ip() {
        IpAddr::V4(ip) => {
            let text = std::fs::read_to_string(PathBuf::from("/proc/net/arp"));
            let Ok(table) = text else {
                return None;
            };
            mac_from_arp_table(&table, &ip.to_string())
        }
        IpAddr::V6(_) => None,
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(idx): Path<usize>,
    Form(form): Form<ReservationForm>,
) -> Response {
    submit_reservation(state, idx, None, form, ConnectInfo(peer)).await
}

async fn update_reservation(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((idx, ridx)): Path<(usize, usize)>,
    Form(form): Form<ReservationForm>,
) -> Response {
    submit_reservation(state, idx, Some(ridx), form, ConnectInfo(peer)).await
}

/// 新增（ridx=None）或修改（ridx=Some）共用的提交流程：表單校驗 → 衝突檢查 → 寫檔 → 帶警告重新導向。
async fn submit_reservation(
    state: Shared,
    idx: usize,
    ridx: Option<usize>,
    form: ReservationForm,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let (parsed, form_errors) = reservation_from_form(&form);
    let host_mac = peer_mac(&peer);
    let mut st = state.lock().await;
    match st.refresh_external() {
        Refresh::ExternalChanged => {
            let mut hidden = String::new();
            hidden.push_str(&format!(
                r#"<input type="hidden" name="hw_address" value="{}">"#,
                escape(&form.hw_address)
            ));
            hidden.push_str(&format!(
                r#"<input type="hidden" name="ip_address" value="{}">"#,
                escape(&form.ip_address)
            ));
            if let Some(h) = form.hostname.as_ref() {
                hidden.push_str(&format!(
                    r#"<input type="hidden" name="hostname" value="{}">"#,
                    escape(h)
                ));
            }
            let action = match ridx {
                Some(ri) => format!("/subnet/{idx}/res/{ri}/edit"),
                None => format!("/subnet/{idx}/new"),
            };
            let intercept = external_decision_page(
                &action,
                Some(&hidden),
                &reservation_form_summary(ridx, &form),
                &format!("/subnet/{idx}"),
                st.saves_since_apply,
            );
            if let Some(resp) = resolve_external(&mut st, form.external_resolve.as_deref(), intercept) {
                return resp;
            }
        }
        _ => {}
    }
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
                Html(page(
                    "表單不合法",
                    &form_html(idx, ridx, Some(&dummy), &form_errors, &[], host_mac.clone()),
                )),
            )
                .into_response();
        }
    };
    let conflicts = check_conflicts(&subnet, &reservation, ridx);
    if !conflicts.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(page(
                "衝突",
                &form_html(
                    idx,
                    ridx,
                    Some(&reservation),
                    &conflict_messages(&conflicts),
                    &[],
                    host_mac.clone(),
                ),
            )),
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

async fn new_form(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(idx): Path<usize>,
) -> Response {
    let mut st = state.lock().await;
    let refresh = st.refresh_external();
    if st.file.subnet(idx).is_err() {
        return page_500("subnet 不存在");
    }
    let next = format!("/subnet/{idx}/new");
    let body = format!(
        "{}{}{}",
        tab_strip("hosts", &format!("/subnet/{idx}")),
        external_banner_html(&refresh, st.saves_since_apply, &next),
        form_html(idx, None, None, &[], &[], peer_mac(&peer)),
    );
    (StatusCode::OK, Html(page("新增", &body))).into_response()
}

async fn edit_form(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((idx, ridx)): Path<(usize, usize)>,
) -> Response {
    let mut st = state.lock().await;
    let refresh = st.refresh_external();
    let subnet = match st.file.subnet(idx) {
        Ok(s) => s,
        Err(_) => return page_500("subnet 不存在"),
    };
    match subnet.reservations.get(ridx) {
        Some(r) => {
            let next = format!("/subnet/{idx}/res/{ridx}/edit");
            let body = format!(
                "{}{}{}",
                tab_strip("hosts", &format!("/subnet/{idx}")),
                external_banner_html(&refresh, st.saves_since_apply, &next),
                form_html(idx, Some(ridx), Some(r), &[], &[], peer_mac(&peer)),
            );
            (StatusCode::OK, Html(page("編輯", &body))).into_response()
        }
        None => page_500("reservation 不存在"),
    }
}

/// 決策頁顯示的 reservation 操作摘要（值均已 escape）。
fn reservation_form_summary(ridx: Option<usize>, form: &ReservationForm) -> String {
    let action = if ridx.is_some() { "編輯" } else { "新增" };
    let host_part = form
        .hostname
        .as_ref()
        .map(|h| format!("、hostname「{}」", escape(h)))
        .unwrap_or_default();
    format!(
        "將{action} reservation：hw-address「{hw}」、ip-address「{ip}」{host}",
        action = action,
        hw = escape(&form.hw_address),
        ip = escape(&form.ip_address),
        host = host_part,
    )
}

async fn delete_reservation(
    State(state): State<Shared>,
    Path((idx, ridx)): Path<(usize, usize)>,
    Query(q): Query<ExternalResolveQuery>,
) -> Response {
    let mut st = state.lock().await;
    match st.refresh_external() {
        Refresh::ExternalChanged => {
            let intercept = external_decision_page(
                &format!("/subnet/{idx}/res/{ridx}/delete"),
                None,
                "將刪除一筆 reservation。",
                &format!("/subnet/{idx}"),
                st.saves_since_apply,
            );
            if let Some(resp) = resolve_external(&mut st, q.external_resolve.as_deref(), intercept) {
                return resp;
            }
        }
        _ => {}
    }
    if let Err(e) = st.file.delete_reservation(idx, ridx) {
        return page_500(&e.to_string());
    }
    if let Err(e) = st.save_locked() {
        return page_500(&e.to_string());
    }
    Redirect::to(&format!("/subnet/{idx}")).into_response()
}

async fn apply_reload(
    State(state): State<Shared>,
    Query(q): Query<ExternalResolveQuery>,
) -> Response {
    let mut st = state.lock().await;
    match st.refresh_external() {
        Refresh::ExternalChanged => {
            let intercept = external_decision_page(
                "/apply",
                None,
                "將把變更套用至 kea。",
                "/subnet/0",
                st.saves_since_apply,
            );
            if let Some(resp) = resolve_external(&mut st, q.external_resolve.as_deref(), intercept) {
                return resp;
            }
        }
        _ => {}
    }
    let Some(cs) = st.file.control_socket() else {
        return Html(r#"<span class="err">設定檔未宣告 control-socket，無法套用</span>"#.to_string())
            .into_response();
    };
    if st.saves_since_apply == 0 {
        return Html(r#"<span class="ok">沒有待套用的變更</span>"#.to_string()).into_response();
    }
    match reload(&cs) {
        Ok(text) => {
            st.saves_since_apply = 0;
            Html(ok_reload_html(&text)).into_response()
        }
        Err(e) => Html(format!(
            r#"<span class="err">檔案已寫入，但 reload 失敗：{}</span>"#,
            escape(&e.to_string())
        )).into_response(),
    }
}

/// 套用成功回應：主要訊息進 hx-target，另以 OOB 片段移除「尚未套用」徽章。
fn ok_reload_html(text: &str) -> String {
    format!(
        r##"<span class="ok">已套用並 reload 成功：{}</span><span id="pending-badge" hx-swap-oob="outerHTML"></span>"##,
        escape(text),
    )
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
.hint {{ color:#68707a; font-size:.85rem; }}
.badge {{ background:#d3f0d3; border-radius:8px; padding:.1rem .5rem; font-size:.8rem; }}
.dim {{ color:#68707a; }}
nav.tabs {{ display:flex; gap:.5rem; align-items:center; margin-bottom:1rem; flex-wrap:wrap; }}
.inline-form {{ display:inline; }}
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
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    fn fixture() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/kea-dhcp4.conf")
    }

    /// 測試用：以目前磁碟狀態建立基準 stat。
    fn baseline_of(p: &std::path::Path) -> Option<FileStat> {
        match file_stat(p) {
            Ok(s) => s,
            Err(_) => None,
        }
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
            baseline_stat: baseline_of(&p),
        };
        (
            app(state).layer(MockConnectInfo(SocketAddr::from((
                [198, 51, 100, 99],
                12345,
            )))),
            p,
        )
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

    #[test]
    fn arp_table_lookup_hits_and_misses() {
        let table = "IP address       HW type     Flags       HW address            Mask     Device\n\
10.1.1.2         0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0\n\
10.1.1.3         0x1         0x0         00:00:00:00:00:00     *        eth0";
        assert_eq!(
            mac_from_arp_table(&table, "10.1.1.2"),
            Some("aa:bb:cc:dd:ee:ff".into())
        );
        assert!(
            mac_from_arp_table(&table, "10.1.1.3").is_none(),
            "incomplete（00:00:00:00:00:00）應視為查無"
        );
        assert!(
            mac_from_arp_table(&table, "10.9.9.9").is_none(),
            "無命中的 IP 應回 None"
        );
    }

    #[test]
    fn form_renders_peer_mac_hint() {
        let html = form_html(0, None, None, &[], &[], Some("aa:bb:cc:dd:ee:ff".into()));
        assert!(
            html.contains("目前連線主機 MAC：aa:bb:cc:dd:ee:ff（可供手動填入）"),
            "form: {html}"
        );
        let html = form_html(0, None, None, &[], &[], None);
        assert!(
            html.contains("無法取得連線主機 MAC（需與本系統同一層網路）"),
            "form: {html}"
        );
    }

    #[tokio::test]
    async fn edit_form_shows_peer_mac_hint() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0/res/0/edit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("連線主機 MAC"),
            "編輯表單應顯示連線主機 MAC 的提示或查無提示：{body}"
        );
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
    async fn searched_list_edit_urls_use_real_indexes() {
        let (app, p) = test_app();
        let f = KeaFile::load(&p).unwrap();
        let mut needled = None;
        for (i, r) in f.subnet(0).unwrap().reservations.iter().enumerate() {
            if r.hostname.as_deref() == Some("chufang-mg4670-3") {
                needled = Some(i);
                break;
            }
        }
        let ridx = needled.expect("fixture 應含 chufang-mg4670-3");
        let (status, body) = get(&app, "/subnet/0?q=mg4670-3").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!("/subnet/0/res/{ridx}/edit")),
            "編輯應指向真實索引 {ridx}：{body}"
        );
        assert!(
            body.contains(&format!("/subnet/0/res/{ridx}/delete")),
            "刪除應指向真實索引 {ridx}：{body}"
        );
        assert!(
            !body.contains(&format!("/subnet/0/res/{}/edit", ridx + 1)),
            "不得以過濾後位置充當索引：{body}"
        );
        assert!(
            !body.contains(&format!("/subnet/0/res/{}/delete", ridx + 1)),
            "不得以過濾後位置充當索引：{body}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
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
    async fn sort_by_ip_orders_numerically() {
        let (app, p) = test_app();
        let f = KeaFile::load(&p).unwrap();
        let mut ips: Vec<(u32, String)> = Vec::new();
        for r in f.subnet(0).unwrap().reservations {
            ips.push((u32::from(r.ip_address), r.hw_address.clone()));
        }
        ips.sort_by_key(|(v, _)| *v);
        let (_, hw_min) = &ips[0];
        let (_, hw_snd) = &ips[1];
        let (status, body) = get(&app, "/subnet/0?sort=ip-address").await;
        assert_eq!(status, StatusCode::OK);
        let pos_min = body.find(hw_min.as_str()).expect("最小 IP 應出現於排序頁");
        let pos_snd = body
            .find(hw_snd.as_str())
            .expect("第二小 IP 應出現於排序頁");
        assert!(
            pos_min < pos_snd,
            "ip-address 應依數值排序：{hw_min} 須先於 {hw_snd}，body: {body}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn sort_header_cycles_and_marks() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0?sort=hostname").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("hostname ▲"), "asc 應標 ▲：{body}");
        assert!(!body.contains("hostname ▼"));
        assert!(
            body.contains(r##"href="/subnet/0?sort=hostname&dir=desc""##),
            "升冪再點應切到 desc：{body}"
        );
        let (_, body) = get(&app, "/subnet/0?sort=hostname&dir=desc").await;
        assert!(body.contains("hostname ▼"), "desc 應標 ▼：{body}");
        assert!(
            body.contains(r##"href="/subnet/0" hx-get="/subnet/0""##),
            "第三態（再點回無排序）不應帶 sort 參數：{body}"
        );
        let (status, body) = get(&app, "/subnet/0?sort=evil").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(" ▲") && !body.contains(" ▼"),
            "無效 sort 應忽略且無標記：{body}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn search_input_keeps_current_sort() {
        let (app, dir) = test_app();
        let (status, body) = get(&app, "/subnet/0?q=chufang&sort=ip-address&dir=desc").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"hx-vals='{"sort":"ip-address","dir":"desc"}'"#),
            "即時搜尋請求應帶當前排序：{body}"
        );
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
        assert_eq!(reloaded.subnet(0).unwrap().reservations.len(), 240);
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
        assert_eq!(s.reservations.len(), 239);
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
        assert_eq!(reloaded.subnet(0).unwrap().reservations.len(), 238);
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
        let state = AppState {
            kea_path: p.clone(),
            backup_keep: 3,
            file: f,
            saves_since_apply: 1,
            baseline_stat: baseline_of(&p),
        };
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

    #[test]
    fn apply_ok_response_clears_pending_badge_via_oob() {
        let html = ok_reload_html("Configuration successful.");
        assert!(
            html.contains(r##"<span id="pending-badge" hx-swap-oob="outerHTML"></span>"##),
            "ok 回應應含 OOB 片段：{html}"
        );
    }

    #[tokio::test]
    async fn pending_badge_marked_for_oob_swap() {
        let (app, dir) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A22&ip_address=10.1.9.22",
        )
        .await;
        let (_, body) = get(&app, "/subnet/0").await;
        assert!(
            body.contains(r##"<span id="pending-badge" class="badge">1 筆變更尚未套用</span>"##),
            "badge 應有固定 id 供 OOB 置換：{body}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// 模擬外部系統：直接以 KeaFile 修改磁碟上的設定檔（不經 AppState）。
    fn externally_edit(path: &std::path::Path, hw: &str, ip: &str) {
        let mut f = KeaFile::load(path).unwrap();
        f.add_reservation(
            0,
            &Reservation {
                hw_address: hw.to_string(),
                ip_address: Ipv4Addr::from_str(ip).unwrap(),
                hostname: Some("external-edit".into()),
            },
        )
        .unwrap();
        std::fs::write(path, crate::file::serialize_pretty3(&f.root)).unwrap();
    }

    #[tokio::test]
    async fn external_change_silently_reloads_when_no_pending() {
        let (app, p) = test_app();
        externally_edit(&p, "aa:bb:cc:dd:ee:61", "10.1.9.61");
        let (status, body) = get(&app, "/subnet/0?q=external-edit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("10.1.9.61"), "無 pending 時外部修改應靜默重載：{body}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn pending_external_change_shows_decision_banner_on_get() {
        let (app, p) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A62&ip_address=10.1.9.62",
        )
        .await;
        externally_edit(&p, "aa:bb:cc:dd:ee:63", "10.1.9.63");
        let (status, body) = get(&app, "/subnet/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("設定檔已被外部修改"), "body: {body}");
        assert!(body.contains("捨棄變更重新載入"), "body: {body}");
        assert!(body.contains("以本系統覆寫"), "body: {body}");
        assert!(
            !body.contains("external-edit"),
            "有 pending 時不得重載：{body}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn pending_external_change_intercepts_post() {
        let (app, p) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A62&ip_address=10.1.9.62",
        )
        .await;
        externally_edit(&p, "aa:bb:cc:dd:ee:63", "10.1.9.63");
        let (status, body) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A64&ip_address=10.1.9.64",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("設定檔已被外部修改"), "應回傳決策頁：{body}");
        assert!(body.contains(r#"name="external_resolve" value="adopt""#), "body: {body}");
        assert!(body.contains(r#"name="external_resolve" value="overwrite""#), "body: {body}");
        assert!(body.contains("aa:bb:cc:dd:ee:64"), "決策頁應保留原始輸入：{body}");
        let reloaded = KeaFile::load(&p).unwrap();
        let hws = reloaded.reservation_hwaddrs();
        assert!(
            !hws.contains(&"aa:bb:cc:dd:ee:64".into()),
            "攔截時不得寫入：{hws:?}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn resubmit_with_overwrite_commits_mutation() {
        let (app, p) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A62&ip_address=10.1.9.62",
        )
        .await;
        externally_edit(&p, "aa:bb:cc:dd:ee:63", "10.1.9.63");
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A64&ip_address=10.1.9.64&external_resolve=overwrite",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        let hws = reloaded.reservation_hwaddrs();
        assert!(hws.contains(&"aa:bb:cc:dd:ee:62".into()), "先前變更應保留：{hws:?}");
        assert!(hws.contains(&"aa:bb:cc:dd:ee:64".into()), "本次變更應寫入：{hws:?}");
        assert!(
            !hws.contains(&"aa:bb:cc:dd:ee:63".into()),
            "覆寫後外部版本應被取代：{hws:?}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn resubmit_with_adopt_reloads_then_commits() {
        let (app, p) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A62&ip_address=10.1.9.62",
        )
        .await;
        externally_edit(&p, "aa:bb:cc:dd:ee:63", "10.1.9.63");
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A64&ip_address=10.1.9.64&external_resolve=adopt",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let reloaded = KeaFile::load(&p).unwrap();
        let hws = reloaded.reservation_hwaddrs();
        assert!(hws.contains(&"aa:bb:cc:dd:ee:63".into()), "adopt 後外部版本應被採用：{hws:?}");
        assert!(hws.contains(&"aa:bb:cc:dd:ee:64".into()), "本次變更應在外部基底上寫入：{hws:?}");
        assert!(
            hws.contains(&"aa:bb:cc:dd:ee:62".into()),
            "adopt 只捨棄「未套用」狀態；已寫回的變更仍留在磁碟：{hws:?}"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn missing_file_warns_but_edits_still_work() {
        let (app, p) = test_app();
        std::fs::remove_file(&p).unwrap();
        let (status, body) = get(&app, "/subnet/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("設定檔已被刪除"), "body: {body}");
        let (status, _) = post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A65&ip_address=10.1.9.65",
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "檔案不存在時仍應可編輯（寫回重建）");
        let reloaded = KeaFile::load(&p).unwrap();
        assert!(reloaded.reservation_hwaddrs().contains(&"aa:bb:cc:dd:ee:65".into()));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn pages_share_tab_strip() {
        let (app, dir) = test_app();
        let (_, body) = get(&app, "/subnet/0").await;
        assert!(body.contains(r##"<nav class="tabs">"##), "子網頁應有分頁列：{body}");
        assert!(body.contains(r##"href="/leases">作用中租用</a>"##), "body: {body}");
        let (_, body) = get(&app, "/leases").await;
        assert!(body.contains(r##"<nav class="tabs">"##), "租用頁應有分頁列：{body}");
        assert!(body.contains(r##"href="/subnet/0">保留主機</a>"##), "body: {body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// 建立租用分頁測試環境：kea 設定檔的 lease-database.name 指向同目錄 CSV，
    /// 並寫入樣本租用檔。
    fn lease_test_app() -> (Router, std::path::PathBuf, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kealight-lease-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kea-dhcp4.conf");
        std::fs::copy(fixture(), &p).unwrap();
        let lease_path = dir.join("kea-leases4.csv");
        let mut f = KeaFile::load(&p).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("name".into(), Value::String(lease_path.to_string_lossy().to_string().into()));
        std::fs::write(&p, crate::file::serialize_pretty3(&f.root)).unwrap();
        std::fs::write(&lease_path, concat!(
            "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context,pool_id\n",
            "10.1.9.9,1c:69:7a:77:3b:98,,3600,9999999999,1,1,1,host-a,0,,0\n",
            "10.1.1.13,,41:32:33:34:35:36,3600,9999999999,1,1,1,no-hw,0,,0\n",
            "10.1.6.30,,,3600,9999999999,1,1,1,resv,0,,0\n",
            "10.1.8.185,,,3600,1000,1,1,1,old-lease,0,,0",
        ))
        .unwrap();
        let state = AppState {
            kea_path: p.clone(),
            backup_keep: 3,
            file: KeaFile::load(&p).unwrap(),
            saves_since_apply: 0,
            baseline_stat: baseline_of(&p),
        };
        (app(state), p, lease_path)
    }

    #[tokio::test]
    async fn leases_page_renders_rows_filters_and_badges() {
        let (app, dir, _) = lease_test_app();
        let (status, body) = get(&app, "/leases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("10.1.9.9"), "body: {body}");
        assert!(body.contains("10.1.6.30"), "body: {body}");
        assert!(body.contains("10.1.0.0/16"), "subnet_id 應對應 CIDR：{body}");
        assert!(body.contains("已指派"), "state 應中文化：{body}");
        assert!(body.contains("剩餘"), "到期應顯示剩餘時間：{body}");
        assert!(body.contains(">保留<"), "對應 reservation 的租用應有保留徽章：{body}");
        assert!(body.contains("作用中"), "預設應顯示作用中：{body}");
        let (_, body) = get(&app, "/leases?state=all").await;
        assert!(body.contains("host-a"), "全部含 host-a：{body}");
        assert!(body.contains("old-lease"), "全部含過期列：{body}");
        let (_, body) = get(&app, "/leases?q=no-hw").await;
        assert!(!body.contains("10.1.9.9"), "搜尋應過濾：{body}");
        assert!(body.contains("10.1.1.13"), "body: {body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn leases_page_tolerates_empty_query_params() {
        let (app, dir, _) = lease_test_app();
        let (status, _) = get(&app, "/leases?state=expired&subnet=&q=").await;
        assert_eq!(status, StatusCode::OK, "空 subnet/q 不得回 400");
        let (status, body) = get(&app, "/leases?sort=address&dir=asc&state=expired&subnet=&q=").await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn leases_page_hides_expired_by_default_and_labels_them() {
        let (app, dir, _) = lease_test_app();
        let (_, body) = get(&app, "/leases").await;
        assert!(!body.contains("old-lease"), "預設（作用中）不應含過期列：{body}");
        let (_, body) = get(&app, "/leases?state=expired").await;
        assert!(body.contains("old-lease"), "過期篩選應含過期列：{body}");
        assert!(
            body.contains("10.1.8.185") && body.contains("已過期"),
            "過期列應顯示「已過期」標籤：{body}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test]
    async fn form_pages_refresh_and_show_external_banner() {
        let (app, p) = test_app();
        post_form(
            &app,
            "/subnet/0/new",
            "hw_address=aa%3Abb%3Acc%3Add%3Aee%3A62&ip_address=10.1.9.62",
        )
        .await;
        externally_edit(&p, "aa:bb:cc:dd:ee:63", "10.1.9.63");
        let (status, body) = get(&app, "/subnet/0/res/0/edit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("設定檔已被外部修改"), "表單頁也應顯示決策橫幅：{body}");
        assert!(body.contains(r##"action="/external/adopt""##), "Table body: {body}");
        assert!(body.contains(r##"action="/external/overwrite""##), "body: {body}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[tokio::test]
    async fn leases_page_reports_non_memfile_and_missing_file() {
        let (_r, dir) = test_app();
        // name 指向一個確定不存在的路徑 → 「租用檔不存在」空狀態（不依賴部署環境）。
        let missing = std::env::temp_dir().join(format!("kealight-nope-{}", std::process::id()));
        let mut f = KeaFile::load(&dir).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("name".into(), Value::String(missing.to_string_lossy().to_string().into()));
        let baseline = baseline_of(&dir);
        let state = AppState {
            kea_path: dir.clone(),
            backup_keep: 3,
            file: f,
            saves_since_apply: 0,
            baseline_stat: baseline,
        };
        let app_missing = app(state);
        let (status, body) = get(&app_missing, "/leases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("租用檔不存在"), "name 指向不存在的檔案應顯示空狀態：{body}");

        let mut f = KeaFile::load(&dir).unwrap();
        f.root
            .get_mut("Dhcp4")
            .unwrap()
            .get_mut("lease-database")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("type".into(), Value::String("mysql".into()));
        std::fs::write(&dir, crate::file::serialize_pretty3(&f.root)).unwrap();
        let baseline = baseline_of(&dir);
        let cleanup = dir.clone();
        let state = AppState {
            kea_path: dir,
            backup_keep: 3,
            file: f,
            saves_since_apply: 0,
            baseline_stat: baseline,
        };
        let app2 = app(state);
        let (_, body) = get(&app2, "/leases").await;
        assert!(body.contains("非 memfile"), "非 memfile 應顯示空狀態：{body}");
        let _ = std::fs::remove_dir_all(&cleanup);
    }
}
