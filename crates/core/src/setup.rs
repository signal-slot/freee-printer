//! The interactive freee login. I/O goes through [`Console`], so the same
//! steps run in a terminal and on a device's serial console.

use anyhow::{Result, anyhow, bail};

use crate::freee::Freee;

/// Line-oriented terminal.
pub trait Console {
    fn write(&mut self, text: &str);
    /// Reads one line without its line ending; `None` at end of input.
    /// `echo` is false for secrets.
    fn read_line(&mut self, echo: bool) -> Option<String>;
}

fn ask(console: &mut dyn Console, prompt: &str, echo: bool) -> Result<String> {
    console.write(prompt);
    let line = console
        .read_line(echo)
        .ok_or_else(|| anyhow!("入力が途中で終わりました"))?;
    Ok(line.trim().to_string())
}

/// Asks for the app credentials, walks the user through authorization and
/// picks the company to upload to.
pub fn freee_login(console: &mut dyn Console, freee: &Freee) -> Result<()> {
    if freee.has_credentials() {
        let id = freee.client_id().unwrap_or_default();
        let answer = ask(
            console,
            &format!("保存済みの freee アプリ (Client ID {id}) を使いますか? [Y/n] "),
            true,
        )?;
        if answer.eq_ignore_ascii_case("n") {
            ask_credentials(console, freee)?;
        }
    } else {
        console.write(
            "freee のアプリ管理 (https://app.secure.freee.co.jp/developers/applications) で\n\
             登録したアプリの Client ID と Client Secret を入力してください。\n",
        );
        ask_credentials(console, freee)?;
    }

    let url = freee.authorize_url()?;
    console.write(&format!(
        "ブラウザで次の URL を開いて、freee でアプリを許可してください:\n\n  {url}\n\n"
    ));
    let code = ask(console, "表示された認可コード: ", true)?;
    if code.is_empty() {
        bail!("認可コードが入力されませんでした");
    }
    let selected = freee.exchange_code(&code)?;

    choose_company(console, freee, selected)?;
    Ok(())
}

/// Lists the companies the token can reach and stores the chosen one.
/// `preferred` is taken without asking when it is among them.
pub fn choose_company(
    console: &mut dyn Console,
    freee: &Freee,
    preferred: Option<u64>,
) -> Result<()> {
    let companies = freee.companies()?;
    let company = match preferred.and_then(|id| companies.iter().find(|c| c.id == id)) {
        Some(company) => company,
        None => match companies.as_slice() {
            [] => bail!("利用できる事業所がありません"),
            [only] => only,
            _ => {
                console.write("アップロード先の事業所を選んでください:\n");
                for (i, company) in companies.iter().enumerate() {
                    console.write(&format!(
                        "  {}) {} (ID {})\n",
                        i + 1,
                        company.label(),
                        company.id
                    ));
                }
                let choice = ask(console, "番号: ", true)?;
                let index: usize = choice
                    .parse()
                    .map_err(|_| anyhow!("番号を入力してください"))?;
                companies
                    .get(index.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("範囲外の番号です"))?
            }
        },
    };
    freee.set_company(company)?;
    console.write(&format!(
        "アップロード先: {} (ID {})\n",
        company.label(),
        company.id
    ));
    Ok(())
}

fn ask_credentials(console: &mut dyn Console, freee: &Freee) -> Result<()> {
    let client_id = ask(console, "Client ID: ", true)?;
    let client_secret = ask(console, "Client Secret: ", false)?;
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Client ID と Client Secret の両方が必要です");
    }
    freee.set_credentials(&client_id, &client_secret)
}
