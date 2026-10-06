//! Starting the printer automatically at login: a systemd user service on
//! Linux, a launchd agent on macOS, a Run registry entry on Windows.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const NAME: &str = "freee-printer";
/// Set in the environment of the registered service so that a running
/// instance can tell whether it is the service itself.
pub const SERVICE_ENV: &str = "FREEE_PRINTER_SERVICE";

pub fn running_as_service() -> bool {
    std::env::var_os(SERVICE_ENV).is_some()
}

fn executable() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    // Windows canonical paths carry a `\\?\` prefix that shells trip over.
    let exe = PathBuf::from(exe.to_string_lossy().trim_start_matches(r"\\?\"));
    if exe.components().any(|c| c.as_os_str() == "target") {
        println!(
            "注意: {} はビルドディレクトリの中です。`cargo install --path cli` で入れたものを登録するほうが安全です。",
            exe.display()
        );
    }
    Ok(exe)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("{program} を実行できません"))?;
    if !status.success() {
        bail!("{program} {} が失敗しました ({status})", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    fn unit_path() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("ホームディレクトリが特定できません")?;
        Ok(base
            .config_dir()
            .join("systemd/user")
            .join(format!("{NAME}.service")))
    }

    pub fn install() -> Result<()> {
        let exe = executable()?;
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Unit]\nDescription=freee file box printer\nAfter=network-online.target\n\n\
                 [Service]\nEnvironment={SERVICE_ENV}=1\nExecStart={} serve\nRestart=on-failure\nRestartSec=5\n\n\
                 [Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )
        .with_context(|| format!("{} に書けません", path.display()))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run(
            "systemctl",
            &["--user", "enable", &format!("{NAME}.service")],
        )?;
        println!(
            "{} を登録しました。ログは `journalctl --user -u {NAME} -f` で見られます。",
            path.display()
        );
        println!("ログインしていない間も動かすには `loginctl enable-linger` を実行してください。");
        Ok(())
    }

    pub fn start() -> Result<()> {
        run(
            "systemctl",
            &["--user", "start", &format!("{NAME}.service")],
        )
    }

    pub fn installed() -> bool {
        unit_path().is_ok_and(|p| p.exists())
    }

    /// `stop_now` also stops a running service; otherwise it keeps running
    /// until it exits and is simply not started again.
    pub fn uninstall(stop_now: bool) -> Result<()> {
        let path = unit_path()?;
        if !path.exists() {
            println!("自動起動は登録されていません。");
            return Ok(());
        }
        let unit = format!("{NAME}.service");
        let args: Vec<&str> = if stop_now {
            vec!["--user", "disable", "--now", &unit]
        } else {
            vec!["--user", "disable", &unit]
        };
        run("systemctl", &args)?;
        std::fs::remove_file(&path)?;
        run("systemctl", &["--user", "daemon-reload"])?;
        println!("{} を外しました。", path.display());
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const LABEL: &str = "io.signal-slot.freee-printer";

    fn home() -> Result<PathBuf> {
        Ok(directories::BaseDirs::new()
            .context("ホームディレクトリが特定できません")?
            .home_dir()
            .to_path_buf())
    }

    fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn domain() -> Result<String> {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .context("id -u を実行できません")?;
        Ok(format!(
            "gui/{}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }

    pub fn install() -> Result<()> {
        let exe = executable()?;
        let path = plist_path()?;
        let log = home()?.join("Library/Logs").join(format!("{NAME}.log"));
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::create_dir_all(log.parent().unwrap())?;
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        std::fs::write(
            &path,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\n\
                 <key>Label</key><string>{LABEL}</string>\n\
                 <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>\n\
                 <key>EnvironmentVariables</key><dict><key>{SERVICE_ENV}</key><string>1</string></dict>\n\
                 <key>RunAtLoad</key><true/>\n\
                 <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
                 <key>StandardOutPath</key><string>{log}</string>\n\
                 <key>StandardErrorPath</key><string>{log}</string>\n\
                 </dict></plist>\n",
                escape(&exe.display().to_string()),
                log = escape(&log.display().to_string())
            ),
        )
        .with_context(|| format!("{} に書けません", path.display()))?;
        let domain = domain()?;
        // A stale registration from an earlier install is harmless to remove.
        Command::new("launchctl")
            .args(["bootout", &domain, &path.display().to_string()])
            .output()
            .ok();
        run(
            "launchctl",
            &["bootstrap", &domain, &path.display().to_string()],
        )?;
        println!(
            "{} を登録しました。ログは {} に出ます。",
            path.display(),
            log.display()
        );
        Ok(())
    }

    /// RunAtLoad does not fire when bootstrapped from a shell, so start it explicitly.
    pub fn start() -> Result<()> {
        run(
            "launchctl",
            &["kickstart", &format!("{}/{LABEL}", domain()?)],
        )
    }

    pub fn installed() -> bool {
        plist_path().is_ok_and(|p| p.exists())
    }

    pub fn uninstall(stop_now: bool) -> Result<()> {
        let path = plist_path()?;
        if !path.exists() {
            println!("自動起動は登録されていません。");
            return Ok(());
        }
        if stop_now {
            run(
                "launchctl",
                &["bootout", &domain()?, &path.display().to_string()],
            )?;
        } else {
            // Forget the registration; the running job ends on its own.
            Command::new("launchctl")
                .args(["disable", &format!("{}/{LABEL}", domain()?)])
                .output()
                .ok();
        }
        std::fs::remove_file(&path)?;
        println!("{} を外しました。", path.display());
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn install() -> Result<()> {
        let exe = executable()?;
        // Started through PowerShell so that no console window stays open.
        let script = format!(
            "$env:{SERVICE_ENV}='1'; Start-Process -WindowStyle Hidden -FilePath '{}' -ArgumentList 'serve'",
            exe.display().to_string().replace('\'', "''")
        );
        let command =
            format!("powershell.exe -NoProfile -WindowStyle Hidden -Command \"{script}\"");
        run(
            "reg",
            &[
                "add", RUN_KEY, "/v", NAME, "/t", "REG_SZ", "/d", &command, "/f",
            ],
        )?;
        println!("ログイン時に起動するよう登録しました (レジストリの Run キー)。");
        Ok(())
    }

    pub fn start() -> Result<()> {
        let exe = std::env::current_exe()?;
        let script = format!(
            "$env:{SERVICE_ENV}='1'; Start-Process -WindowStyle Hidden -FilePath '{}' -ArgumentList 'serve'",
            exe.display().to_string().replace('\'', "''")
        );
        run(
            "powershell.exe",
            &["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script],
        )
    }

    pub fn installed() -> bool {
        Command::new("reg")
            .args(["query", RUN_KEY, "/v", NAME])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub fn uninstall(_stop_now: bool) -> Result<()> {
        run("reg", &["delete", RUN_KEY, "/v", NAME, "/f"])?;
        println!("自動起動の登録を外しました。動いているものは手で終了してください。");
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub fn install() -> Result<()> {
        bail!("この OS では自動起動の登録に対応していません")
    }

    pub fn start() -> Result<()> {
        bail!("この OS では自動起動の登録に対応していません")
    }

    pub fn installed() -> bool {
        false
    }

    pub fn uninstall(_stop_now: bool) -> Result<()> {
        bail!("この OS では自動起動の登録に対応していません")
    }
}

pub use imp::{install, installed, start, uninstall};

/// Whether this OS has an autostart mechanism we know how to drive.
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos", windows))
}
