//! Registers the printer with the OS so that it shows up in print dialogs.
//! Linux and macOS go through CUPS (`lpadmin`); Windows through PowerShell's
//! `Add-Printer`, which is untested here, with instructions as the fallback.

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// The queue name; what users pick in print dialogs.
pub const QUEUE: &str = "freee";

fn silent(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn available() -> bool {
    if cfg!(windows) {
        silent(
            "powershell.exe",
            &["-NoProfile", "-Command", "Get-Command Add-Printer"],
        )
    } else {
        Command::new("lpstat")
            .arg("-r")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }
}

pub fn installed() -> bool {
    if cfg!(windows) {
        silent(
            "powershell.exe",
            &[
                "-NoProfile",
                "-Command",
                &format!("Get-Printer -Name '{QUEUE}'"),
            ],
        )
    } else {
        silent("lpstat", &["-p", QUEUE])
    }
}

/// The URI the queue currently points at.
pub fn device_uri() -> Option<String> {
    if cfg!(windows) {
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &format!("(Get-PrinterPort -Name (Get-Printer -Name '{QUEUE}').PortName).PrinterHostAddress")])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (!text.is_empty()).then(|| text.replacen("http://", "ipp://", 1));
    }
    let output = Command::new("lpstat").args(["-v", QUEUE]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    // "device for freee: ipp://..." in the current locale.
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .find(|word| word.starts_with("ipp://") || word.starts_with("ipps://"))
        .map(str::to_string)
}

/// Adds the queue, or points an existing one at `uri`, asking for privileges
/// through sudo when CUPS refuses. The printer must be running: CUPS reads
/// its attributes to set a new queue up, and a failed attempt leaves no queue.
pub fn install(uri: &str, info: &str) -> Result<()> {
    if cfg!(windows) {
        // Add-Printer needs administrator rights: run it elevated, which shows
        // the UAC prompt, and wait for it.
        let url = uri.replacen("ipp://", "http://", 1);
        println!(
            "プリンターの追加には管理者権限が必要です。確認のダイアログで「はい」を選んでください。"
        );
        let inner = format!("Add-Printer -Name ''{QUEUE}'' -IppURL ''{url}''");
        let script = format!(
            "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-Command','{inner}'"
        );
        let status = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .status()?;
        if !status.success() {
            bail!("管理者としての Add-Printer が実行できませんでした ({status})");
        }
        if !installed() {
            bail!("Add-Printer が失敗しました。設定画面から手で追加してください");
        }
        return Ok(());
    }
    if let Some((host, port)) = uri
        .strip_prefix("ipp://")
        .and_then(|rest| rest.split('/').next()?.rsplit_once(':'))
        && std::net::TcpStream::connect((host, port.parse::<u16>().unwrap_or(631))).is_err()
    {
        bail!("{uri} に接続できません。プリンターが動いている状態で実行してください");
    }
    let args: Vec<&str> = if installed() {
        // Only the address changes; the queue keeps its driver settings.
        vec!["-p", QUEUE, "-E", "-v", uri]
    } else {
        vec!["-p", QUEUE, "-E", "-v", uri, "-m", "everywhere", "-D", info]
    };
    let direct = Command::new("lpadmin").args(&args).output()?;
    if direct.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&direct.stderr);
    if !message.contains("Forbidden") && !message.to_lowercase().contains("not authorized") {
        bail!("lpadmin: {}", message.trim());
    }
    println!(
        "CUPS にプリンターを追加するには管理者権限が必要です。sudo のパスワードを求められることがあります。"
    );
    let status = Command::new("sudo").arg("lpadmin").args(&args).status()?;
    if !status.success() {
        bail!("sudo lpadmin が失敗しました ({status})");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let direct = Command::new("lpadmin").args(["-x", QUEUE]).output()?;
    if direct.status.success() {
        return Ok(());
    }
    let status = Command::new("sudo")
        .args(["lpadmin", "-x", QUEUE])
        .status()?;
    if !status.success() {
        bail!("sudo lpadmin -x が失敗しました ({status})");
    }
    Ok(())
}

/// What to do by hand when the queue could not be added automatically.
pub fn instructions(uri: &str) -> String {
    if cfg!(windows) {
        format!(
            "Windows の「設定 → Bluetooth とデバイス → プリンターとスキャナー → デバイスの追加 → 手動で追加」で\n\
             「共有プリンターを名前で選択する」に次を入力してください:\n\n  {}\n",
            uri.replacen("ipp://", "http://", 1)
        )
    } else {
        format!(
            "次のコマンドでプリンターを追加してください:\n\n  sudo lpadmin -p {QUEUE} -E -v {uri} -m everywhere\n"
        )
    }
}
