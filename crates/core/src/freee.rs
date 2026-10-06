//! freee OAuth2 and the file box (receipts) API.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::net::{Dialer, Store};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::http::{self, Origin};

/// Upload limit of the file box API.
pub const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;

/// The redirect URI of apps that show the authorization code on screen.
pub const REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

/// Refresh this long before the access token actually expires.
const EXPIRY_MARGIN_SECS: i64 = 300;
const UPLOAD_ATTEMPTS: u32 = 3;

// Store keys. They are the variable names of the `~/.config/freee/credentials`
// file that other freee tools on the same machine read with the shell, and
// they stay within the 15 characters NVS on the ESP32 allows.
pub const KEY_CLIENT_ID: &str = "CLIENT_ID";
pub const KEY_CLIENT_SECRET: &str = "CLIENT_SECRET";
pub const KEY_ACCESS: &str = "ACCESS_TOKEN";
pub const KEY_REFRESH: &str = "REFRESH_TOKEN";
pub const KEY_EXPIRES: &str = "TOKEN_EXPIRES";
pub const KEY_COMPANY_ID: &str = "COMPANY_ID";
pub const KEY_COMPANY_NAME: &str = "COMPANY_NAME";

pub struct Freee {
    dialer: Arc<dyn Dialer>,
    store: Arc<dyn Store>,
    accounts: Origin,
    api: Origin,
    /// freee refresh tokens are single use, so refreshes must not overlap.
    refresh: Mutex<()>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    company_id: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Company {
    pub id: u64,
    pub display_name: Option<String>,
    pub name: Option<String>,
}

impl Company {
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.name.as_deref())
            .unwrap_or("")
    }
}

pub struct Upload {
    pub file_name: String,
    pub mime: &'static str,
    pub data: Vec<u8>,
    pub description: String,
    /// receipt / invoice / other; left to freee's OCR when `None`.
    pub document_type: Option<String>,
}

/// An error that will not go away by retrying the same request.
#[derive(Debug)]
struct Permanent(anyhow::Error);

impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Permanent {}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Freee {
    pub fn new(dialer: Arc<dyn Dialer>, store: Arc<dyn Store>) -> Self {
        Freee::with_origins(
            dialer,
            store,
            "https://accounts.secure.freee.co.jp",
            "https://api.freee.co.jp",
        )
        .expect("built-in URLs are valid")
    }

    /// Points the client at other servers; tests use a mock.
    pub fn with_origins(
        dialer: Arc<dyn Dialer>,
        store: Arc<dyn Store>,
        accounts: &str,
        api: &str,
    ) -> Result<Self> {
        let parse = |url: &str| Origin::parse(url).ok_or_else(|| anyhow!("不正な URL です: {url}"));
        Ok(Freee {
            dialer,
            store,
            accounts: parse(accounts)?,
            api: parse(api)?,
            refresh: Mutex::new(()),
        })
    }

    pub fn client_id(&self) -> Option<String> {
        self.store.get(KEY_CLIENT_ID)
    }

    pub fn set_credentials(&self, client_id: &str, client_secret: &str) -> Result<()> {
        self.store.set(KEY_CLIENT_ID, client_id)?;
        self.store.set(KEY_CLIENT_SECRET, client_secret)?;
        Ok(())
    }

    pub fn has_credentials(&self) -> bool {
        self.store.get(KEY_CLIENT_ID).is_some() && self.store.get(KEY_CLIENT_SECRET).is_some()
    }

    fn credentials(&self) -> Result<(String, String)> {
        match (
            self.store.get(KEY_CLIENT_ID),
            self.store.get(KEY_CLIENT_SECRET),
        ) {
            (Some(id), Some(secret)) => Ok((id, secret)),
            _ => Err(anyhow!(Permanent(anyhow!(
                "freee アプリの Client ID と Client Secret が未設定です"
            )))),
        }
    }

    pub fn is_logged_in(&self) -> bool {
        self.store.get(KEY_REFRESH).is_some()
    }

    /// Unix time at which the current access token expires.
    pub fn token_expiry(&self) -> Option<i64> {
        self.store.get(KEY_EXPIRES)?.parse().ok()
    }

    pub fn company(&self) -> Option<(u64, String)> {
        let id = self.store.get(KEY_COMPANY_ID)?.parse().ok()?;
        Some((id, self.store.get(KEY_COMPANY_NAME).unwrap_or_default()))
    }

    pub fn set_company(&self, company: &Company) -> Result<()> {
        self.store.set(KEY_COMPANY_ID, &company.id.to_string())?;
        self.store.set(KEY_COMPANY_NAME, company.label())?;
        Ok(())
    }

    /// The page where the user authorizes the app and gets a code.
    pub fn authorize_url(&self) -> Result<String> {
        let (client_id, _) = self.credentials()?;
        let query = http::form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT_URI),
            ("prompt", "select_company"),
        ]);
        Ok(self.accounts.url(&format!("/public_api/authorize?{query}")))
    }

    /// Exchanges an authorization code for tokens and stores them.
    /// Returns the company selected on the consent screen, if freee reported one.
    pub fn exchange_code(&self, code: &str) -> Result<Option<u64>> {
        let (client_id, client_secret) = self.credentials()?;
        let response = self.token_request(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
        ])?;
        let company_id = response.company_id.as_ref().and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        });
        self.save_tokens(response)?;
        Ok(company_id)
    }

    fn token_request(&self, params: &[(&str, &str)]) -> Result<TokenResponse> {
        let body = http::form(params);
        let (status, response) = http::request(
            self.dialer.as_ref(),
            &self.accounts,
            "POST",
            "/public_api/token",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            &[body.as_bytes()],
        )
        .context("freee の認証サーバーに接続できません")?;
        let text = String::from_utf8_lossy(&response);
        if !(200..300).contains(&status) {
            let error = anyhow!(
                "トークンを取得できません ({status}): {}",
                error_message(&text)
            );
            // 5xx may be temporary; anything else means the grant is no longer valid.
            return Err(if (400..500).contains(&status) {
                anyhow!(Permanent(error))
            } else {
                error
            });
        }
        serde_json::from_str(&text).context("トークン応答を解釈できません")
    }

    fn save_tokens(&self, response: TokenResponse) -> Result<String> {
        // The refresh token goes first: losing it would require a new login.
        self.store.set(KEY_REFRESH, &response.refresh_token)?;
        self.store.set(KEY_ACCESS, &response.access_token)?;
        self.store
            .set(KEY_EXPIRES, &(now() + response.expires_in).to_string())?;
        Ok(response.access_token)
    }

    /// Returns a valid access token, refreshing it when needed. `rejected` is
    /// a token the API just refused, which forces a refresh.
    fn access_token(&self, rejected: Option<&str>) -> Result<String> {
        let _guard = self.refresh.lock().unwrap();
        let _shared = self.store.lock()?;
        let relogin = || {
            anyhow!(Permanent(anyhow!(
                "freee にログインしていません。セットアップをやり直してください"
            )))
        };
        let refresh_token = self.store.get(KEY_REFRESH).ok_or_else(relogin)?;
        if let Some(access) = self.store.get(KEY_ACCESS) {
            // Without a recorded expiry (another tool wrote the token) it is
            // tried as is; a 401 brings us back here with `rejected` set.
            let fresh = self
                .token_expiry()
                .is_none_or(|at| at - now() > EXPIRY_MARGIN_SECS);
            // Another thread or process may already have replaced the rejected token.
            if fresh && rejected != Some(access.as_str()) {
                return Ok(access);
            }
        }
        let (client_id, client_secret) = self.credentials()?;
        let response = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", &refresh_token),
            ])
            .map_err(|e| match e.downcast::<Permanent>() {
                Ok(Permanent(e)) => {
                    anyhow!(Permanent(e.context(
                        "freee への再ログインが必要です。セットアップをやり直してください"
                    )))
                }
                Err(e) => e,
            })?;
        self.save_tokens(response)
    }

    /// Sends an authenticated request, refreshing the token once on 401.
    fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[&[u8]],
    ) -> Result<(u16, String)> {
        let mut token = self.access_token(None)?;
        for attempt in 0..2 {
            let bearer = format!("Bearer {token}");
            let mut all = vec![("Authorization", bearer.as_str())];
            all.extend_from_slice(headers);
            let (status, response) =
                http::request(self.dialer.as_ref(), &self.api, method, path, &all, body)
                    .context("freee API に接続できません")?;
            if status == 401 && attempt == 0 {
                token = self.access_token(Some(&token))?;
                continue;
            }
            return Ok((status, String::from_utf8_lossy(&response).into_owned()));
        }
        unreachable!()
    }

    pub fn companies(&self) -> Result<Vec<Company>> {
        #[derive(Deserialize)]
        struct Companies {
            companies: Vec<Company>,
        }
        let (status, body) = self.send("GET", "/api/1/companies", &[], &[])?;
        if status != 200 {
            bail!(
                "事業所一覧を取得できません ({status}): {}",
                error_message(&body)
            );
        }
        Ok(serde_json::from_str::<Companies>(&body)
            .context("事業所一覧を解釈できません")?
            .companies)
    }

    /// Uploads a document to the file box and returns the receipt id.
    pub fn upload(&self, upload: &Upload) -> Result<u64> {
        if upload.data.len() > MAX_FILE_SIZE {
            bail!(
                "ファイルが大きすぎます ({:.1} MB)。ファイルボックスの上限は 64 MB です",
                upload.data.len() as f64 / 1048576.0
            );
        }
        let (company_id, _) = self
            .company()
            .ok_or_else(|| anyhow!("アップロード先の事業所が未設定です"))?;
        let mut attempt = 1;
        loop {
            match self.upload_once(company_id, upload) {
                Ok(id) => return Ok(id),
                Err(e) => match e.downcast::<Permanent>() {
                    Ok(Permanent(e)) => return Err(e),
                    Err(e) if attempt >= UPLOAD_ATTEMPTS => return Err(e),
                    Err(e) => {
                        log::warn!("アップロード失敗 ({attempt}/{UPLOAD_ATTEMPTS}): {e:#}");
                        std::thread::sleep(Duration::from_secs(3 * attempt as u64));
                        attempt += 1;
                    }
                },
            }
        }
    }

    fn upload_once(&self, company_id: u64, upload: &Upload) -> Result<u64> {
        let boundary = format!("freee-printer-{}", uuid::Uuid::new_v4().simple());
        let field = |name: &str, value: &str| {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
        };
        let mut head = field("company_id", &company_id.to_string());
        if !upload.description.is_empty() {
            head.push_str(&field("description", &upload.description));
        }
        if let Some(document_type) = &upload.document_type {
            head.push_str(&field("document_type", document_type));
        }
        // The file name goes out as plain UTF-8, like a browser sends it.
        head.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"receipt\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            upload.file_name.replace(['"', '\r', '\n'], "_"),
            upload.mime
        ));
        let tail = format!("\r\n--{boundary}--\r\n");
        let content_type = format!("multipart/form-data; boundary={boundary}");

        let (status, body) = self.send(
            "POST",
            "/api/1/receipts",
            &[("Content-Type", &content_type)],
            &[head.as_bytes(), &upload.data, tail.as_bytes()],
        )?;
        if (200..300).contains(&status) {
            let json: serde_json::Value =
                serde_json::from_str(&body).context("アップロード応答を解釈できません")?;
            return json["receipt"]["id"].as_u64().ok_or_else(|| {
                anyhow!(Permanent(anyhow!(
                    "アップロード応答に receipt.id がありません: {body}"
                )))
            });
        }
        let error = anyhow!(
            "アップロードに失敗しました ({status}): {}",
            error_message(&body)
        );
        if status >= 500 || status == 429 {
            Err(error)
        } else {
            Err(anyhow!(Permanent(error)))
        }
    }
}

/// Pulls the human readable part out of a freee error body.
fn error_message(body: &str) -> String {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.trim().chars().take(300).collect();
    };
    let mut messages: Vec<String> = Vec::new();
    if let Some(errors) = json["errors"].as_array() {
        for error in errors {
            if let Some(list) = error["messages"].as_array() {
                messages.extend(list.iter().filter_map(|m| m.as_str()).map(str::to_string));
            }
        }
    }
    for key in ["message", "error_description", "error"] {
        if let Some(message) = json[key].as_str() {
            messages.push(message.to_string());
        }
    }
    if messages.is_empty() {
        body.trim().chars().take(300).collect()
    } else {
        messages.join(" / ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(
                r#"{"status_code":400,"errors":[{"type":"validation","messages":["a","b"]}]}"#
            ),
            "a / b"
        );
        assert_eq!(
            error_message(r#"{"error":"invalid_grant","error_description":"期限切れ"}"#),
            "期限切れ / invalid_grant"
        );
        assert_eq!(error_message("<html>oops</html>"), "<html>oops</html>");
    }
}
