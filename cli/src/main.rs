mod cups;
mod mdns;
mod service;
mod store;
mod tls;

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use directories::{BaseDirs, ProjectDirs};
use freee_printer_core::freee::{self, Freee};
use freee_printer_core::net::{Store, std_spawner};
use freee_printer_core::printer::{self, FailedDocuments, Host, Printer, PrinterConfig};
use freee_printer_core::setup::{self, Console};
use freee_printer_core::stream::OsListener;

use store::CredentialsFile;
use tls::HostDialer;

/// "FREE" in lookalike digits (F=7, R=9, E=3), like 35932 is "ESP32".
const DEFAULT_LISTEN: &str = "127.0.0.1:7933";

// Store keys next to the ones the freee client keeps.

/// 印刷すると freee のファイルボックスにアップロードされる仮想プリンター
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// 設定を置くディレクトリ
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// 省略すると、freee に (未ログインなら) ログインしてプリンターとして動く
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// freee にログインして、アップロード先の事業所を選ぶ
    Login,
    /// アップロード先の事業所を選び直す (ログインし直さずに)
    Company {
        /// 事業所 ID。省略すると一覧から選ぶ
        id: Option<u64>,
    },
    /// プリンターとして動かす
    Serve {
        /// 待ち受けアドレス (既定: 127.0.0.1:7933)
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// --listen のアドレスを DNS-SD で LAN に告知する
        #[arg(long)]
        advertise: bool,
    },
    /// プリンターを介さずにファイルを直接アップロードする
    Upload {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// 設定とログイン状態を表示する
    Status,
    /// この OS の印刷システムにプリンターを登録する (`serve` を動かした状態で)
    Install {
        /// 登録するプリンターの URI (既定: 動いているプリンターのもの)
        #[arg(long)]
        uri: Option<String>,
    },
    /// この OS の印刷システムからプリンターを外す
    Uninstall,
    /// ログイン時に自動で起動するよう登録する (Linux の systemd ユーザーサービス)
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// 登録して今すぐ起動する
    Install,
    /// 登録を外して止める
    Uninstall,
}

struct Paths {
    /// Shared with other freee tools on this machine; see `store.rs`.
    credentials: PathBuf,
    /// Documents that could not be uploaded are kept here.
    failed: PathBuf,
}

impl Paths {
    fn new(config_dir: Option<&Path>) -> Result<Self> {
        let (config, data) = match config_dir {
            Some(dir) => (dir.to_path_buf(), dir.to_path_buf()),
            None => {
                let base =
                    BaseDirs::new().ok_or_else(|| anyhow!("ホームディレクトリが特定できません"))?;
                let dirs = ProjectDirs::from("io", "signal-slot", "freee-printer")
                    .ok_or_else(|| anyhow!("ホームディレクトリが特定できません"))?;
                (
                    base.config_dir().join("freee"),
                    dirs.data_dir().to_path_buf(),
                )
            }
        };
        Ok(Paths {
            credentials: config.join("credentials"),
            failed: data.join("failed"),
        })
    }
}

struct App {
    paths: Paths,
    store: Arc<CredentialsFile>,
    freee: Arc<Freee>,
}

impl App {
    fn open(config_dir: Option<&Path>) -> Result<Self> {
        let paths = Paths::new(config_dir)?;
        let store = Arc::new(CredentialsFile::open(&paths.credentials)?);
        let dialer = Arc::new(HostDialer::new());
        // The overrides exist so that tests can point at a mock server.
        let freee = match (
            std::env::var("FREEE_ACCOUNTS_URL"),
            std::env::var("FREEE_API_URL"),
        ) {
            (Ok(accounts), Ok(api)) => {
                Freee::with_origins(dialer.clone(), store.clone(), &accounts, &api)?
            }
            _ => Freee::new(dialer.clone(), store.clone()),
        };
        Ok(App {
            paths,
            store,
            freee: Arc::new(freee),
        })
    }

    fn is_set_up(&self) -> bool {
        self.freee.is_logged_in() && self.freee.company().is_some()
    }

    fn require_login(&self) -> Result<()> {
        if !self.is_set_up() {
            bail!("freee にログインしていません。先に `freee-printer login` を実行してください");
        }
        Ok(())
    }
}

/// Failed documents are plain files in the data directory.
struct FailedDir(PathBuf);

impl FailedDocuments for FailedDir {
    fn keep(&self, name: &str, data: &[u8]) {
        let path = self.0.join(name);
        match std::fs::create_dir_all(&self.0).and_then(|_| std::fs::write(&path, data)) {
            Ok(()) => log::warn!("処理できなかった文書を保存しました: {}", path.display()),
            Err(e) => log::error!("文書を {} に保存できません: {e}", path.display()),
        }
    }

    fn list(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort_by(|a, b| b.cmp(a));
        names
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(name)).ok()
    }

    fn remove(&self, name: &str) {
        if let Err(e) = std::fs::remove_file(self.0.join(name)) {
            log::warn!("{name} を削除できません: {e}");
        }
    }
}

/// The autostart switch on the settings page.
struct Autostart {
    stop: Arc<AtomicBool>,
    /// Set when this instance should start the service once it has stopped serving.
    handover: Arc<AtomicBool>,
}

impl Host for Autostart {
    fn autostart(&self) -> Option<bool> {
        service::supported().then(service::installed)
    }

    fn set_autostart(&self, on: bool) -> Result<String> {
        if !on {
            service::uninstall(false)?;
            return Ok(
                "ログイン時の自動起動をやめました。今動いているものはそのまま動きます。"
                    .to_string(),
            );
        }
        if service::installed() {
            return Ok("自動起動は登録済みです。".to_string());
        }
        service::install()?;
        if service::running_as_service() {
            return Ok("自動起動を登録しました。".to_string());
        }
        // This instance holds the port; it steps aside and then starts the service.
        self.handover.store(true, Ordering::Relaxed);
        let stop = self.stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            stop.store(true, Ordering::Relaxed);
        });
        Ok("自動起動を登録しました。今動いているこのプロセスは終了し、数秒後にサービスが引き継ぎます。少し待ってから再読み込みしてください。".to_string())
    }
}

/// This machine's name, which distinguishes printers from each other.
fn host_name() -> String {
    #[cfg(unix)]
    {
        gethostname::gethostname().to_string_lossy().into_owned()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "freee-printer".to_string())
    }
}

struct Terminal;

impl Console for Terminal {
    fn write(&mut self, text: &str) {
        print!("{text}");
        std::io::stdout().flush().ok();
    }

    fn read_line(&mut self, _echo: bool) -> Option<String> {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(n) if n > 0 => Some(line.trim_end_matches(['\r', '\n']).to_string()),
            _ => None,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Interactive steps speak for themselves.
    let default_filter = match cli.command {
        Some(Command::Login | Command::Company { .. }) => "warn",
        _ => "info",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .init();

    let config_dir = cli.config_dir.as_deref();
    match cli.command {
        None => serve(config_dir, None, false, true),
        Some(Command::Login) => setup::freee_login(&mut Terminal, &App::open(config_dir)?.freee),
        Some(Command::Company { id }) => {
            setup::choose_company(&mut Terminal, &App::open(config_dir)?.freee, id)
        }
        Some(Command::Serve { listen, advertise }) => serve(config_dir, listen, advertise, false),
        Some(Command::Upload { files }) => upload(config_dir, files),
        Some(Command::Status) => status(config_dir),
        Some(Command::Install { uri }) => install(config_dir, uri),
        Some(Command::Service(ServiceCommand::Install)) => {
            service::install().and_then(|_| service::start())
        }
        Some(Command::Service(ServiceCommand::Uninstall)) => service::uninstall(true),
        Some(Command::Uninstall) => {
            cups::uninstall()?;
            println!("プリンター「{}」を外しました。", cups::QUEUE);
            Ok(())
        }
    }
}

/// The printer URI other programs on this machine should use.
fn local_uri() -> String {
    format!(
        "ipp://localhost:{}/ipp/print",
        DEFAULT_LISTEN.rsplit(':').next().unwrap_or_default()
    )
}

fn install(config_dir: Option<&Path>, uri: Option<String>) -> Result<()> {
    let app = App::open(config_dir)?;
    let uri = uri.unwrap_or_else(local_uri);
    if !cups::available() {
        println!("{}", cups::instructions(&uri));
        return Ok(());
    }
    if cups::installed() && cups::device_uri().is_none_or(|current| current == uri) {
        println!("プリンター「{}」は登録済みです。", cups::QUEUE);
        return Ok(());
    }
    let info = app
        .store
        .get(printer::KEY_NAME)
        .unwrap_or_else(|| PrinterConfig::default().name);
    cups::install(&uri, &info)?;
    println!(
        "プリンター「{}」を登録しました。印刷ダイアログで選ぶと、ファイルボックスにアップロードされます。",
        cups::QUEUE
    );
    Ok(())
}

/// Tells the user what the running printer is good for and, on a machine
/// with CUPS, offers to register it so that it appears in print dialogs.
fn welcome(uri: String, web: String, info: String, set_up: bool, interactive: bool) {
    std::thread::spawn(move || {
        // Let CUPS find the printer up and the log lines settle first.
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("\nプリンター「{info}」が動いています。状態と設定: {web}");
        if !set_up {
            let setup = format!("{web}setup");
            println!("freee へのログインがまだです。ブラウザで {setup} を開いて進めてください。");
            if interactive && open::that(&setup).is_err() {
                println!("(ブラウザを自動で開けませんでした)");
            }
        }
        if cups::available() {
            if cups::installed() {
                let current = cups::device_uri();
                // Windows does not report the address of an IPP queue; trust it.
                if current.is_none() || current.as_deref() == Some(uri.as_str()) {
                    println!(
                        "印刷ダイアログで「{}」を選ぶと、ファイルボックスにアップロードされます。",
                        cups::QUEUE
                    );
                    return;
                }
                println!(
                    "登録済みのプリンター「{}」は {} を指していて、今の {uri} と違います。",
                    cups::QUEUE,
                    current.unwrap_or_default()
                );
                if !interactive {
                    println!("`freee-printer install` で更新できます。");
                    return;
                }
                print!("更新しますか? [Y/n] ");
                std::io::stdout().flush().ok();
                let answer = Terminal.read_line(true).unwrap_or_default();
                if answer.trim().eq_ignore_ascii_case("n") {
                    return;
                }
                match cups::install(&uri, &info) {
                    Ok(()) => println!("更新しました。"),
                    Err(e) => println!("更新できませんでした: {e:#}"),
                }
                return;
            }
            if interactive {
                print!(
                    "このマシンの印刷ダイアログに「{}」を追加しますか? [Y/n] ",
                    cups::QUEUE
                );
                std::io::stdout().flush().ok();
                let answer = Terminal.read_line(true).unwrap_or_default();
                if !answer.trim().eq_ignore_ascii_case("n") {
                    match cups::install(&uri, &info) {
                        Ok(()) => {
                            println!(
                                "追加しました。印刷ダイアログで「{}」を選ぶと、ファイルボックスにアップロードされます。",
                                cups::QUEUE
                            );
                            return;
                        }
                        Err(e) => println!("追加できませんでした: {e:#}"),
                    }
                }
            }
        }
        println!("{}", cups::instructions(&uri));
    });
}

fn serve(
    config_dir: Option<&Path>,
    listen: Option<SocketAddr>,
    advertise: bool,
    interactive: bool,
) -> Result<()> {
    let app = App::open(config_dir)?;

    let listen: SocketAddr = listen.unwrap_or(DEFAULT_LISTEN.parse()?);
    if advertise && listen.ip().is_loopback() {
        bail!(
            "DNS-SD で告知するには `--listen 0.0.0.0:7933` のように外部から届くアドレスで待ち受けてください"
        );
    }

    let config = PrinterConfig {
        identity: host_name(),
        ..Default::default()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let handover = Arc::new(AtomicBool::new(false));
    let printer = Printer::new(
        config,
        app.freee.clone(),
        app.store.clone(),
        std_spawner(),
        Some(Box::new(FailedDir(app.paths.failed.clone()))),
        Some(Box::new(Autostart {
            stop: stop.clone(),
            handover: handover.clone(),
        })),
    );
    let name = printer.name();

    let listener =
        OsListener::bind(listen).with_context(|| format!("{listen} で待ち受けできません"))?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        log::warn!(
            "{addr} で待ち受けます。このアドレスに届く誰でもファイルボックスにアップロードできます"
        );
    }
    let host = if addr.ip().is_unspecified() || addr.ip().is_loopback() {
        "localhost".to_string()
    } else {
        addr.ip().to_string()
    };
    log::info!("プリンター URI: ipp://{host}:{}/ipp/print", addr.port());
    if interactive {
        welcome(
            format!("ipp://{host}:{}/ipp/print", addr.port()),
            format!("http://localhost:{}/", addr.port()),
            name.clone(),
            app.is_set_up(),
            std::io::IsTerminal::is_terminal(&std::io::stdin()),
        );
    }
    let advertisement = if advertise {
        Some(mdns::advertise(&name, addr.port(), printer.uuid())?)
    } else {
        None
    };

    let handler_stop = stop.clone();
    ctrlc::set_handler(move || handler_stop.store(true, Ordering::Relaxed))?;

    printer.serve(&listener, &stop);
    drop(advertisement);
    drop(listener);
    if handover.load(Ordering::Relaxed) {
        // The port is free now; the registered service takes over.
        service::start()?;
        println!("自動起動のサービスに引き継ぎました。");
    }
    Ok(())
}

fn upload(config_dir: Option<&Path>, files: Vec<PathBuf>) -> Result<()> {
    let app = App::open(config_dir)?;
    app.require_login()?;
    for path in files {
        let data =
            std::fs::read(&path).with_context(|| format!("{} を読めません", path.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (mut upload, _) = printer::prepare(None, false, true, data, &name)
            .map_err(|(e, _)| e.context(path.display().to_string()))?;
        upload.document_type = app.store.get(printer::KEY_DOCUMENT_TYPE);
        let receipt = app
            .freee
            .upload(&upload)
            .with_context(|| path.display().to_string())?;
        println!(
            "{} → {} (receipt id {receipt})",
            path.display(),
            upload.file_name
        );
    }
    Ok(())
}

fn status(config_dir: Option<&Path>) -> Result<()> {
    let app = App::open(config_dir)?;
    println!("設定ファイル: {}", app.paths.credentials.display());
    let get = |key: &str| app.store.get(key);

    match get(freee::KEY_CLIENT_ID) {
        Some(id) => println!("freee アプリ: Client ID {id}"),
        None => {
            println!("freee アプリ: 未設定 (`freee-printer` を実行するとログインから始まります)")
        }
    }
    match (get(freee::KEY_REFRESH), get(freee::KEY_COMPANY_ID)) {
        (Some(_), Some(id)) => {
            println!(
                "freee: ログイン済み、事業所 {} (ID {id})",
                get(freee::KEY_COMPANY_NAME).unwrap_or_default()
            )
        }
        _ => println!("freee: 未ログイン"),
    }
    let printer_uri = "ipp://localhost:7933/ipp/print".to_string();
    println!("プリンター URI: {printer_uri}");
    println!("失敗した文書の保存先: {}", app.paths.failed.display());
    Ok(())
}
