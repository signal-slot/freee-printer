//! The web pages: status for everyone, setup and settings for whoever sits at
//! this machine. Plain HTML forms, no scripts, so that it works everywhere a
//! browser does and on a small device.

use std::sync::Arc;

use crate::freee::REDIRECT_URI;
use crate::http::{self, Request, Response};
use crate::printer::{KEY_DOCUMENT_TYPE, KEY_NAME, Printer, prepare};

pub struct JobView {
    pub id: i32,
    pub time: String,
    pub name: String,
    pub state: &'static str,
    pub message: String,
}

const STYLE: &str = "body{font-family:sans-serif;max-width:52em;margin:2em auto;padding:0 1em;line-height:1.6;color:#222}\
h1{font-size:1.4em}h2{font-size:1.1em;margin-top:2em}table{border-collapse:collapse;width:100%}\
th,td{border-bottom:1px solid #ddd;padding:.4em .5em;text-align:left;vertical-align:top}th{background:#f5f5f5}\
nav a{margin-right:1.2em}form.inline{display:inline}input[type=text],input[type=password],select{width:100%;max-width:28em;padding:.3em}\
label{display:block;margin-top:.8em}button{padding:.3em .9em}.notice{background:#fff4d6;border:1px solid #e8c56a;padding:.6em 1em;border-radius:4px}\
.ok{background:#e6f6e6;border:1px solid #8fcf8f;padding:.6em 1em;border-radius:4px}.err{background:#fde8e8;border:1px solid #f0a0a0;padding:.6em 1em;border-radius:4px}\
code{background:#f3f3f3;padding:0 .3em}.muted{color:#666}";

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, local: bool, body: &str) -> Response {
    let nav = if local {
        "<nav><a href=\"/\">状態</a><a href=\"/setup\">セットアップ</a><a href=\"/settings\">設定</a></nav>"
    } else {
        "<nav><a href=\"/\">状態</a></nav>"
    };
    Response::html(format!(
        "<!doctype html><html lang=\"ja\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{}</title><link rel=\"icon\" href=\"/icon.png\"><style>{STYLE}</style></head><body>{nav}{body}</body></html>",
        escape(title)
    ))
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> &'a str {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_default()
}

fn query(request: &Request, name: &str) -> String {
    let Some((_, query)) = request.path.split_once('?') else {
        return String::new();
    };
    field(&http::parse_form(query.as_bytes()), name).to_string()
}

fn redirect_with(path: &str, kind: &str, message: &str) -> Response {
    Response::redirect(&format!("{path}?{kind}={}", http::encode(message)))
}

pub fn status(printer: &Arc<Printer>, local: bool, request: &Request) -> Response {
    let mut body = format!("<h1>{}</h1>", escape(&printer.name()));
    let ok = query(request, "ok");
    let err = query(request, "err");
    if !ok.is_empty() {
        body.push_str(&format!("<p class=\"ok\">{}</p>", escape(&ok)));
    }
    if !err.is_empty() {
        body.push_str(&format!("<p class=\"err\">{}</p>", escape(&err)));
    }

    match printer.freee.company() {
        Some((id, name)) if printer.freee.is_logged_in() => {
            body.push_str(&format!(
                "<p>アップロード先: {} <span class=\"muted\">(ID {id})</span></p>",
                escape(if name.is_empty() { "事業所" } else { &name })
            ));
        }
        _ if local => body.push_str(
            "<p class=\"notice\">freee にログインしていません。<a href=\"/setup\">セットアップ</a>を済ませるまで、印刷した文書はアップロードされません。</p>",
        ),
        _ => body.push_str("<p class=\"notice\">freee にログインしていません。</p>"),
    }

    body.push_str("<h2>ジョブ</h2>");
    let jobs = printer.jobs_snapshot();
    if jobs.is_empty() {
        body.push_str("<p class=\"muted\">まだ印刷されていません。印刷ダイアログでこのプリンターを選ぶと、ここに並びます。</p>");
    } else {
        body.push_str(
            "<table><tr><th>ID</th><th>受付</th><th>名前</th><th>状態</th><th>メッセージ</th></tr>",
        );
        for job in jobs {
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                job.id,
                job.time,
                escape(&job.name),
                job.state,
                escape(&job.message)
            ));
        }
        body.push_str("</table>");
    }

    if let Some(failed) = &printer.failed {
        let names = failed.list();
        if !names.is_empty() {
            body.push_str(
                "<h2>アップロードできなかった文書</h2><table><tr><th>ファイル</th><th></th></tr>",
            );
            for name in names {
                let actions = if local {
                    format!(
                        "<form class=\"inline\" method=\"post\" action=\"/failed/resend\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                         <input type=\"hidden\" name=\"name\" value=\"{n}\"><button>送り直す</button></form> \
                         <form class=\"inline\" method=\"post\" action=\"/failed/delete\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                         <input type=\"hidden\" name=\"name\" value=\"{n}\"><button>削除</button></form>",
                        csrf = printer.csrf_token,
                        n = escape(&name)
                    )
                } else {
                    String::new()
                };
                body.push_str(&format!(
                    "<tr><td>{}</td><td>{actions}</td></tr>",
                    escape(&name)
                ));
            }
            body.push_str("</table>");
        }
    }
    page(&printer.name(), local, &body)
}

/// Routes the setup, settings and failed-document requests (local only).
pub fn handle(printer: &Arc<Printer>, request: &Request, path: &str) -> Response {
    if request.method == "GET" {
        return match path {
            "/setup" => setup(printer, request),
            "/settings" => settings(printer, request),
            _ => Response::text(404, "not found"),
        };
    }
    let fields = http::parse_form(&request.body);
    if field(&fields, "csrf") != printer.csrf_token {
        return Response::text(403, "stale form; reload the page");
    }
    match path {
        "/setup/app" => {
            let (id, secret) = (
                field(&fields, "client_id").trim(),
                field(&fields, "client_secret").trim(),
            );
            if id.is_empty() || secret.is_empty() {
                return redirect_with(
                    "/setup",
                    "err",
                    "Client ID と Client Secret の両方を入力してください",
                );
            }
            match printer.freee.set_credentials(id, secret) {
                Ok(()) => Response::redirect("/setup"),
                Err(e) => redirect_with("/setup", "err", &format!("保存できません: {e:#}")),
            }
        }
        "/setup/code" => {
            let code = field(&fields, "code").trim();
            if code.is_empty() {
                return redirect_with("/setup", "err", "認可コードを貼り付けてください");
            }
            match printer.freee.exchange_code(code) {
                Ok(selected) => choose_company(printer, request, selected, None),
                Err(e) => redirect_with("/setup", "err", &format!("{e:#}")),
            }
        }
        "/setup/company" | "/settings/company" => {
            let id = field(&fields, "company").trim().parse::<u64>().ok();
            choose_company(printer, request, id, None)
        }
        "/settings" => {
            let name = field(&fields, "name").trim();
            let document_type = field(&fields, "document_type").trim();
            let saved = printer
                .store
                .set(KEY_NAME, name)
                .and_then(|_| match document_type {
                    "receipt" | "invoice" | "other" => {
                        printer.store.set(KEY_DOCUMENT_TYPE, document_type)
                    }
                    _ => printer.store.remove(KEY_DOCUMENT_TYPE),
                });
            match saved {
                Ok(()) => redirect_with("/", "ok", "設定を保存しました"),
                Err(e) => redirect_with("/settings", "err", &format!("保存できません: {e}")),
            }
        }
        "/settings/autostart" => {
            let Some(host) = &printer.host else {
                return Response::text(404, "not found");
            };
            let on = field(&fields, "autostart") == "on";
            match host.set_autostart(on) {
                Ok(message) => redirect_with("/settings", "ok", &message),
                Err(e) => redirect_with(
                    "/settings",
                    "err",
                    &format!("自動起動を変更できません: {e:#}"),
                ),
            }
        }
        "/failed/resend" | "/failed/delete" => failed_action(printer, path, field(&fields, "name")),
        _ => Response::text(404, "not found"),
    }
}

fn setup(printer: &Arc<Printer>, request: &Request) -> Response {
    let err = query(request, "err");
    let change_app = query(request, "app") == "change";
    let csrf = &printer.csrf_token;
    let mut body = String::from("<h1>セットアップ</h1>");
    if !err.is_empty() {
        body.push_str(&format!("<p class=\"err\">{}</p>", escape(&err)));
    }

    if printer.freee.is_logged_in()
        && let Some((id, name)) = printer.freee.company()
        && !change_app
    {
        body.push_str(&format!(
            "<p class=\"ok\">freee にログイン済みです。アップロード先: {} (ID {id})</p>\
             <p>事業所を変えるには<a href=\"/settings\">設定</a>へ。別のアプリでログインし直すには<a href=\"/setup?app=change\">こちら</a>。</p>",
            escape(&name)
        ));
        return page("セットアップ", true, &body);
    }

    if !printer.freee.has_credentials() || change_app {
        body.push_str(&format!(
            "<h2>1. freee にアプリを登録する</h2>\
             <p><a href=\"https://app.secure.freee.co.jp/developers/applications\" target=\"_blank\">freee アプリ管理</a>で\
             「新規追加」し、次のように設定します。</p>\
             <table><tr><th>アプリタイプ</th><td>プライベート</td></tr>\
             <tr><th>コールバック URL</th><td><code>{redirect}</code>（既定値のまま）</td></tr>\
             <tr><th>権限</th><td>会計の「ファイルボックス」を更新、「事業所」を参照</td></tr></table>\
             <p>作成後に表示される Client ID と Client Secret を入力してください。</p>\
             <form method=\"post\" action=\"/setup/app\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             <label>Client ID <input type=\"text\" name=\"client_id\" value=\"{id}\" required></label>\
             <label>Client Secret <input type=\"password\" name=\"client_secret\" required></label>\
             <p><button>保存して次へ</button></p></form>",
            redirect = REDIRECT_URI,
            id = escape(&printer.freee.client_id().unwrap_or_default())
        ));
        return page("セットアップ", true, &body);
    }

    let authorize = printer.freee.authorize_url().unwrap_or_default();
    body.push_str(&format!(
        "<h2>2. freee で許可する</h2>\
         <p>Client ID <code>{id}</code> のアプリを使います（<a href=\"/setup?app=change\">別のアプリにする</a>）。</p>\
         <p><a href=\"{url}\" target=\"_blank\">freee を開いて許可する</a> と認可コードが表示されるので、ここに貼り付けてください。</p>\
         <form method=\"post\" action=\"/setup/code\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label>認可コード <input type=\"text\" name=\"code\" autocomplete=\"off\" required></label>\
         <p><button>ログイン</button></p></form>",
        id = escape(&printer.freee.client_id().unwrap_or_default()),
        url = escape(&authorize)
    ));
    page("セットアップ", true, &body)
}

/// Stores the company when it is determined, otherwise asks.
fn choose_company(
    printer: &Arc<Printer>,
    _request: &Request,
    preferred: Option<u64>,
    error: Option<&str>,
) -> Response {
    let companies = match printer.freee.companies() {
        Ok(companies) => companies,
        Err(e) => {
            return redirect_with(
                "/setup",
                "err",
                &format!("事業所一覧を取得できません: {e:#}"),
            );
        }
    };
    let chosen = match preferred.and_then(|id| companies.iter().find(|c| c.id == id)) {
        Some(company) => Some(company),
        None if companies.len() == 1 => companies.first(),
        None => None,
    };
    if let Some(company) = chosen {
        return match printer.freee.set_company(company) {
            Ok(()) => redirect_with(
                "/",
                "ok",
                &format!("アップロード先: {} (ID {})", company.label(), company.id),
            ),
            Err(e) => redirect_with("/setup", "err", &format!("保存できません: {e:#}")),
        };
    }
    let mut body = String::from("<h1>アップロード先の事業所</h1>");
    if let Some(error) = error {
        body.push_str(&format!("<p class=\"err\">{}</p>", escape(error)));
    }
    if companies.is_empty() {
        body.push_str("<p class=\"err\">利用できる事業所がありません。</p>");
        return page("事業所", true, &body);
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/setup/company\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">",
        printer.csrf_token
    ));
    for company in &companies {
        body.push_str(&format!(
            "<label><input type=\"radio\" name=\"company\" value=\"{}\" required> {} <span class=\"muted\">(ID {})</span></label>",
            company.id,
            escape(company.label()),
            company.id
        ));
    }
    body.push_str("<p><button>この事業所にする</button></p></form>");
    page("事業所", true, &body)
}

fn settings(printer: &Arc<Printer>, request: &Request) -> Response {
    let err = query(request, "err");
    let ok = query(request, "ok");
    let csrf = &printer.csrf_token;
    let document_type = printer.document_type().unwrap_or_default();
    let option = |value: &str, label: &str| {
        format!(
            "<option value=\"{value}\"{}>{label}</option>",
            if document_type == value {
                " selected"
            } else {
                ""
            }
        )
    };
    let mut body = String::from("<h1>設定</h1>");
    if !ok.is_empty() {
        body.push_str(&format!("<p class=\"ok\">{}</p>", escape(&ok)));
    }
    if !err.is_empty() {
        body.push_str(&format!("<p class=\"err\">{}</p>", escape(&err)));
    }
    if let Some(host) = &printer.host
        && let Some(on) = host.autostart()
    {
        body.push_str(&format!(
            "<h2>自動起動</h2><form method=\"post\" action=\"/settings/autostart\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             <label><input type=\"checkbox\" name=\"autostart\" value=\"on\"{}> ログイン時に自動で起動する</label>\
             <p><button>適用</button></p></form>",
            if on { " checked" } else { "" }
        ));
    }
    body.push_str("<h2>プリンター</h2>");
    body.push_str(&format!(
        "<form method=\"post\" action=\"/settings\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label>印刷ダイアログに出る名前 <input type=\"text\" name=\"name\" value=\"{name}\"></label>\
         <label>書類の種類 <select name=\"document_type\">{auto}{receipt}{invoice}{other}</select></label>\
         <p><button>保存</button></p></form>",
        name = escape(&printer.name()),
        auto = option("", "freee の OCR に任せる"),
        receipt = option("receipt", "領収書"),
        invoice = option("invoice", "請求書"),
        other = option("other", "その他"),
    ));

    body.push_str("<h2>アップロード先の事業所</h2>");
    if !printer.freee.is_logged_in() {
        body.push_str("<p class=\"muted\">freee にログインすると選べます。</p>");
        return page("設定", true, &body);
    }
    let current = printer.freee.company().map(|(id, _)| id);
    match printer.freee.companies() {
        Ok(companies) => {
            body.push_str(&format!(
                "<form method=\"post\" action=\"/settings/company\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">"
            ));
            for company in &companies {
                body.push_str(&format!(
                    "<label><input type=\"radio\" name=\"company\" value=\"{}\"{}> {} <span class=\"muted\">(ID {})</span></label>",
                    company.id,
                    if current == Some(company.id) { " checked" } else { "" },
                    escape(company.label()),
                    company.id
                ));
            }
            body.push_str("<p><button>この事業所にする</button></p></form>");
        }
        Err(e) => body.push_str(&format!(
            "<p class=\"err\">事業所一覧を取得できません: {}</p>",
            escape(&format!("{e:#}"))
        )),
    }
    page("設定", true, &body)
}

fn failed_action(printer: &Arc<Printer>, path: &str, name: &str) -> Response {
    let Some(failed) = &printer.failed else {
        return Response::text(404, "not found");
    };
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return redirect_with("/", "err", "不正なファイル名です");
    }
    if path == "/failed/delete" {
        failed.remove(name);
        return redirect_with("/", "ok", &format!("{name} を削除しました"));
    }
    let Some(data) = failed.read(name) else {
        return redirect_with("/", "err", &format!("{name} が見つかりません"));
    };
    // Saved as "<date>-<time>-<job>-<original name>".
    let original = name.splitn(4, '-').nth(3).unwrap_or(name);
    let stem = original
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(original);
    let result = match prepare(None, false, true, data, stem) {
        Ok((mut upload, _)) => {
            upload.document_type = printer.document_type();
            printer
                .freee
                .upload(&upload)
                .map(|receipt| (receipt, upload.file_name))
        }
        Err((e, _)) => Err(e),
    };
    match result {
        Ok((receipt, file_name)) => {
            failed.remove(name);
            redirect_with(
                "/",
                "ok",
                &format!("{file_name} をアップロードしました (id {receipt})"),
            )
        }
        Err(e) => redirect_with("/", "err", &format!("{name}: {e:#}")),
    }
}
