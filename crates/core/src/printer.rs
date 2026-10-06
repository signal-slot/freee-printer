//! The IPP Everywhere printer that turns print jobs into file box uploads.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{DateTime, Datelike, Local, Timelike};
use uuid::Uuid;

use crate::freee::{Freee, Upload};
use crate::http::{Connection, Request, Response, TooLarge};
use crate::ipp::{self, Attr, Message, Value, keyword, keywords, mime, op, status, tag, text, uri};
use crate::net::{Spawner, Store};
use crate::stream::{Listener, Stream};
use crate::ui;
use crate::{icon, raster};

const MAX_JOB_HISTORY: usize = 100;
const RASTER_DPI: i32 = 300;

const FORMATS: &[&str] = &[
    "application/pdf",
    "image/jpeg",
    "image/png",
    "image/pwg-raster",
    "image/urf",
    "application/octet-stream",
];

/// (PWG media name, width, height) in hundredths of millimetres.
const MEDIA: &[(&str, i32, i32)] = &[
    ("iso_a4_210x297mm", 21000, 29700),
    ("iso_a3_297x420mm", 29700, 42000),
    ("iso_a5_148x210mm", 14800, 21000),
    ("jis_b5_182x257mm", 18200, 25700),
    ("jis_b4_257x364mm", 25700, 36400),
    ("na_letter_8.5x11in", 21590, 27940),
    ("na_legal_8.5x14in", 21590, 35560),
];

const OPERATIONS: &[u16] = &[
    op::PRINT_JOB,
    op::VALIDATE_JOB,
    op::CREATE_JOB,
    op::SEND_DOCUMENT,
    op::CANCEL_JOB,
    op::GET_JOB_ATTRIBUTES,
    op::GET_JOBS,
    op::GET_PRINTER_ATTRIBUTES,
    op::CANCEL_MY_JOBS,
    op::CLOSE_JOB,
    op::IDENTIFY_PRINTER,
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum JobState {
    Pending = 3,
    Processing = 5,
    Canceled = 7,
    Aborted = 8,
    Completed = 9,
}

impl JobState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Canceled | JobState::Aborted | JobState::Completed
        )
    }
}

struct Job {
    id: i32,
    uuid: Uuid,
    name: String,
    user: String,
    state: JobState,
    reason: &'static str,
    message: String,
    /// document-format given at Create-Job, used when Send-Document omits it.
    format: Option<String>,
    pages: Option<i32>,
    created: Stamp,
    processing: Option<Stamp>,
    completed: Option<Stamp>,
    /// The document has arrived and a worker owns the job.
    busy: bool,
}

#[derive(Clone, Copy)]
struct Stamp {
    uptime: i32,
    time: DateTime<Local>,
}

pub struct PrinterConfig {
    /// Name shown in print dialogs.
    pub name: String,
    /// Distinguishes this printer from others; the printer UUID derives from it.
    pub identity: String,
    /// receipt / invoice / other; left to freee's OCR when `None`.
    pub document_type: Option<String>,
    /// Largest request to buffer. Raster jobs are far bigger than the PDF
    /// that gets uploaded.
    pub max_request: usize,
    /// Stack size of connection and job threads.
    pub stack_size: usize,
    /// Convert PWG and Apple raster to PDF. Needs a compressor, which small
    /// devices cannot afford.
    pub accept_raster: bool,
    /// Accept gzip-compressed documents.
    pub accept_gzip: bool,
    /// Jobs that may be in flight at once; further ones are told to retry.
    /// Each holds its document in memory.
    pub max_active_jobs: usize,
}

impl Default for PrinterConfig {
    fn default() -> Self {
        PrinterConfig {
            name: "freee ファイルボックス".to_string(),
            identity: "freee-printer".to_string(),
            document_type: None,
            max_request: 1024 * 1024 * 1024,
            stack_size: 2 * 1024 * 1024,
            accept_raster: true,
            accept_gzip: true,
            max_active_jobs: usize::MAX,
        }
    }
}

/// Keeps documents that could not be converted or uploaded, so that a print
/// is not lost and can be sent again from the web page.
pub trait FailedDocuments: Send + Sync {
    fn keep(&self, name: &str, data: &[u8]);
    /// Names, newest first.
    fn list(&self) -> Vec<String>;
    fn read(&self, name: &str) -> Option<Vec<u8>>;
    fn remove(&self, name: &str);
}

/// What the surrounding program can do for the printer on this computer,
/// offered on the settings page when present.
pub trait Host: Send + Sync {
    /// Whether the printer starts at login; `None` when the host cannot tell.
    fn autostart(&self) -> Option<bool>;
    /// Turns starting at login on or off and says what happened.
    fn set_autostart(&self, on: bool) -> anyhow::Result<String>;
}

// Settings that can be changed while running live in the store.
pub const KEY_NAME: &str = "pr_name";
pub const KEY_DOCUMENT_TYPE: &str = "pr_doc_type";

pub struct Printer {
    config: PrinterConfig,
    pub(crate) freee: Arc<Freee>,
    pub(crate) store: Arc<dyn Store>,
    spawner: Spawner,
    pub(crate) failed: Option<Box<dyn FailedDocuments>>,
    pub(crate) host: Option<Box<dyn Host>>,
    /// Guards the settings forms against requests forged by other web pages.
    pub(crate) csrf_token: String,
    uuid: Uuid,
    started: Instant,
    started_at: DateTime<Local>,
    next_job_id: AtomicI32,
    jobs: Mutex<Vec<Job>>,
}

impl Printer {
    pub fn new(
        config: PrinterConfig,
        freee: Arc<Freee>,
        store: Arc<dyn Store>,
        spawner: Spawner,
        failed: Option<Box<dyn FailedDocuments>>,
        host: Option<Box<dyn Host>>,
    ) -> Arc<Self> {
        let uuid = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("freee-printer://{}", config.identity).as_bytes(),
        );
        Arc::new(Printer {
            config,
            freee,
            store,
            spawner,
            failed,
            host,
            csrf_token: Uuid::new_v4().simple().to_string(),
            uuid,
            started: Instant::now(),
            started_at: Local::now(),
            next_job_id: AtomicI32::new(1),
            jobs: Mutex::new(Vec::new()),
        })
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Name shown in print dialogs; the store overrides the configured one.
    pub fn name(&self) -> String {
        self.store
            .get(KEY_NAME)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.config.name.clone())
    }

    /// receipt / invoice / other, or `None` to leave it to freee's OCR.
    pub fn document_type(&self) -> Option<String> {
        self.store
            .get(KEY_DOCUMENT_TYPE)
            .filter(|t| matches!(t.as_str(), "receipt" | "invoice" | "other"))
            .or_else(|| self.config.document_type.clone())
    }

    pub(crate) fn jobs_snapshot(&self) -> Vec<ui::JobView> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .rev()
            .map(|job| ui::JobView {
                id: job.id,
                time: job.created.time.format("%Y-%m-%d %H:%M:%S").to_string(),
                name: job.name.clone(),
                state: match job.state {
                    JobState::Pending => "待機中",
                    JobState::Processing => "処理中",
                    JobState::Canceled => "取り消し",
                    JobState::Aborted => "失敗",
                    JobState::Completed => "完了",
                },
                message: job.message.clone(),
            })
            .collect()
    }

    /// Accepts connections until `stop` is set.
    pub fn serve(self: &Arc<Self>, listener: &dyn Listener, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            let Some(stream) = listener.accept(Duration::from_millis(500)) else {
                continue;
            };
            let printer = self.clone();
            let spawned = (self.spawner)(
                "ipp-conn",
                self.config.stack_size,
                Box::new(move || printer.connection(stream)),
            );
            if let Err(e) = spawned {
                log::error!("接続を処理するスレッドを起動できません: {e}");
            }
        }
    }

    fn connection(self: Arc<Self>, stream: Box<dyn Stream>) {
        let mut conn = Connection::new(stream);
        // Settings pages are only for whoever sits at this machine.
        let local = conn
            .peer()
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|addr| addr.ip().is_loopback());
        loop {
            match conn.read_request(self.config.max_request) {
                Ok(Some(request)) => {
                    let keep_alive = !request
                        .header("connection")
                        .is_some_and(|v| v.eq_ignore_ascii_case("close"));
                    let response = self.handle(request, local);
                    if conn.respond(&response, keep_alive).is_err() || !keep_alive {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let too_large = e.get_ref().is_some_and(|inner| inner.is::<TooLarge>());
                    let response = if too_large {
                        log::warn!(
                            "{} からの文書が大きすぎます (上限 {} bytes)",
                            conn.peer(),
                            self.config.max_request
                        );
                        Response::text(413, "document too large")
                    } else {
                        log::debug!("{}: {e}", conn.peer());
                        Response::text(400, "bad request")
                    };
                    conn.respond(&response, false).ok();
                    break;
                }
            }
        }
    }

    /// Answers one HTTP request.
    /// Answers one HTTP request. `local` is true for connections from this
    /// machine, which may use the settings pages.
    pub fn handle(self: &Arc<Self>, request: Request, local: bool) -> Response {
        let path = request
            .path
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        let ipp = request
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/ipp"));
        match (request.method.as_str(), path.as_str()) {
            ("POST", _) if ipp => {
                let host = request.header("host").unwrap_or("localhost").to_string();
                let mut body = request.body;
                let (message, offset) = match Message::parse(&body) {
                    Ok(parsed) => parsed,
                    Err(e) => return Response::text(400, &format!("invalid IPP request: {e}")),
                };
                log::debug!("IPP 0x{:04x} via {host}", message.code);
                let document = body.split_off(offset);
                drop(body);
                Response::new(
                    200,
                    "application/ipp",
                    self.dispatch(&host, &message, document).encode(),
                )
            }
            ("GET", "/icon.png") => Response::new(200, "image/png", icon::png()),
            ("GET", "/" | "/ipp/print") => ui::status(self, local, &request),
            ("GET" | "POST", _)
                if path.starts_with("/setup")
                    || path.starts_with("/settings")
                    || path.starts_with("/failed") =>
            {
                if !local {
                    return Response::text(403, "settings are only available from this machine");
                }
                ui::handle(self, &request, &path)
            }
            ("GET", _) => Response::text(404, "not found"),
            _ => Response::text(405, "method not allowed"),
        }
    }

    fn now(&self) -> Stamp {
        // printer-up-time must be at least 1.
        Stamp {
            uptime: self.started.elapsed().as_secs() as i32 + 1,
            time: Local::now(),
        }
    }

    fn dispatch(self: &Arc<Self>, host: &str, request: &Message, document: Vec<u8>) -> Message {
        if !matches!(request.version.0, 1 | 2) {
            return Message::response(request, status::VERSION_NOT_SUPPORTED);
        }
        if request.request_id == 0 {
            return Message::response(request, status::BAD_REQUEST)
                .with_status_message("bad request-id 0");
        }
        // RFC 8011 section 4.1.4: every request starts with these two attributes.
        let operation = request.groups.first().filter(|g| g.tag == tag::OPERATION);
        let leading: Vec<&str> = operation
            .iter()
            .flat_map(|g| g.attrs.iter().take(2))
            .map(|a| a.name.as_str())
            .collect();
        if leading != ["attributes-charset", "attributes-natural-language"] {
            return Message::response(request, status::BAD_REQUEST)
                .with_status_message("missing required operation attributes");
        }
        if request.op_str("printer-uri").is_none() && request.op_str("job-uri").is_none() {
            return Message::response(request, status::BAD_REQUEST)
                .with_status_message("missing printer-uri");
        }
        let result = match request.code {
            op::GET_PRINTER_ATTRIBUTES => Ok(self.get_printer_attributes(host, request)),
            op::VALIDATE_JOB => self
                .validate(request)
                .map(|_| Message::response(request, status::OK)),
            op::PRINT_JOB => self.print_job(host, request, document),
            op::CREATE_JOB => self.create_job(host, request),
            op::SEND_DOCUMENT => self.send_document(host, request, document),
            op::GET_JOB_ATTRIBUTES => self.get_job_attributes(host, request),
            op::GET_JOBS => Ok(self.get_jobs(host, request)),
            op::CANCEL_JOB => self.cancel_job(request),
            op::CANCEL_MY_JOBS => Ok(self.cancel_my_jobs(request)),
            op::CLOSE_JOB => self.close_job(request),
            op::IDENTIFY_PRINTER => Ok(Message::response(request, status::OK)),
            _ => Err((
                status::OPERATION_NOT_SUPPORTED,
                format!("operation 0x{:04x} is not supported", request.code),
            )),
        };
        result.unwrap_or_else(|(code, message)| {
            Message::response(request, code).with_status_message(&message)
        })
    }

    fn formats(&self) -> Vec<&'static str> {
        FORMATS
            .iter()
            .copied()
            .filter(|f| {
                self.config.accept_raster || !matches!(*f, "image/pwg-raster" | "image/urf")
            })
            .collect()
    }

    /// Refuses a new job while as many as this device can hold are in flight.
    fn check_capacity(&self) -> IppResult<()> {
        let active = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| !j.state.is_terminal())
            .count();
        if active >= self.config.max_active_jobs {
            return Err((
                status::BUSY,
                "busy with another job; retry later".to_string(),
            ));
        }
        Ok(())
    }

    /// Checks the operation attributes shared by job creation requests.
    fn validate(&self, request: &Message) -> IppResult<()> {
        if let Some(format) = request.op_str("document-format")
            && !self.formats().contains(&format)
        {
            return Err((
                status::DOCUMENT_FORMAT_NOT_SUPPORTED,
                format!("{format} is not supported"),
            ));
        }
        if let Some(compression) = request.op_str("compression")
            && !(compression == "none" || (compression == "gzip" && self.config.accept_gzip))
        {
            return Err((
                status::COMPRESSION_NOT_SUPPORTED,
                format!("{compression} is not supported"),
            ));
        }
        Ok(())
    }

    fn new_job(&self, request: &Message) -> i32 {
        let id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            uuid: Uuid::new_v4(),
            name: request.op_str("job-name").unwrap_or_default().to_string(),
            user: request
                .op_str("requesting-user-name")
                .unwrap_or("anonymous")
                .to_string(),
            state: JobState::Pending,
            reason: "job-incoming",
            message: String::new(),
            format: request.op_str("document-format").map(str::to_string),
            pages: None,
            created: self.now(),
            processing: None,
            completed: None,
            busy: false,
        };
        let mut jobs = self.jobs.lock().unwrap();
        jobs.push(job);
        // Forget the oldest finished jobs.
        while jobs.len() > MAX_JOB_HISTORY {
            match jobs.iter().position(|j| j.state.is_terminal()) {
                Some(index) => jobs.remove(index),
                None => break,
            };
        }
        id
    }

    fn print_job(
        self: &Arc<Self>,
        host: &str,
        request: &Message,
        document: Vec<u8>,
    ) -> IppResult<Message> {
        self.validate(request)?;
        if document.is_empty() {
            return Err((status::BAD_REQUEST, "no document data".to_string()));
        }
        self.check_capacity()?;
        let id = self.new_job(request);
        self.start(id, request, document);
        Ok(self.job_response(host, request, id))
    }

    fn create_job(&self, host: &str, request: &Message) -> IppResult<Message> {
        self.validate(request)?;
        self.check_capacity()?;
        let id = self.new_job(request);
        Ok(self.job_response(host, request, id))
    }

    fn send_document(
        self: &Arc<Self>,
        host: &str,
        request: &Message,
        document: Vec<u8>,
    ) -> IppResult<Message> {
        self.validate(request)?;
        let id = self.job_id(request)?;
        let last = request
            .find(tag::OPERATION, "last-document")
            .and_then(|a| a.values.first()?.as_bool())
            .ok_or((status::BAD_REQUEST, "missing last-document".to_string()))?;
        {
            let mut jobs = self.jobs.lock().unwrap();
            let job = find_job(&mut jobs, id)?;
            if job.state != JobState::Pending || job.busy {
                return Err((
                    status::NOT_POSSIBLE,
                    "job is not waiting for a document".to_string(),
                ));
            }
            if document.is_empty() {
                if !last {
                    return Err((status::BAD_REQUEST, "no document data".to_string()));
                }
                // Closing a job that never received a document.
                job.state = JobState::Canceled;
                job.reason = "job-canceled-by-user";
                job.completed = Some(self.now());
                drop(jobs);
                return Ok(self.job_response(host, request, id));
            }
        }
        self.start(id, request, document);
        Ok(self.job_response(host, request, id))
    }

    fn close_job(&self, request: &Message) -> IppResult<Message> {
        let id = self.job_id(request)?;
        let mut jobs = self.jobs.lock().unwrap();
        let job = find_job(&mut jobs, id)?;
        if job.state == JobState::Pending && !job.busy {
            job.state = JobState::Canceled;
            job.reason = "job-canceled-by-user";
            job.completed = Some(self.now());
        }
        Ok(Message::response(request, status::OK))
    }

    /// Hands the document to a worker thread; clients poll the job state.
    fn start(self: &Arc<Self>, id: i32, request: &Message, document: Vec<u8>) {
        let format = request.op_str("document-format").map(str::to_string);
        let gzip = request.op_str("compression") == Some("gzip");
        if let Ok(job) = find_job(&mut self.jobs.lock().unwrap(), id) {
            job.busy = true;
        }
        let printer = self.clone();
        let work = Box::new(move || printer.process(id, format, gzip, document));
        if let Err(e) = (self.spawner)("ipp-job", self.config.stack_size, work) {
            self.finish(
                id,
                Err(anyhow!("ジョブを処理するスレッドを起動できません: {e}")),
            );
        }
    }

    fn process(self: Arc<Self>, id: i32, format: Option<String>, gzip: bool, document: Vec<u8>) {
        let (name, format) = {
            let mut jobs = self.jobs.lock().unwrap();
            let Ok(job) = find_job(&mut jobs, id) else {
                return;
            };
            job.state = JobState::Processing;
            job.reason = "job-printing";
            job.processing = Some(self.now());
            (job.name.clone(), format.or_else(|| job.format.clone()))
        };
        log::info!("ジョブ {id} 受信: {name:?} ({} bytes)", document.len());

        let result = match prepare(
            format.as_deref(),
            gzip,
            self.config.accept_raster,
            document,
            &name,
        ) {
            Ok((mut upload, pages)) => {
                if let Ok(job) = find_job(&mut self.jobs.lock().unwrap(), id) {
                    job.pages = pages;
                }
                upload.document_type = self.document_type();
                let result = self.freee.upload(&upload);
                if result.is_err() {
                    self.keep_failed(id, &upload.file_name, &upload.data);
                }
                result.map(|receipt| (receipt, upload.file_name))
            }
            Err((e, document)) => {
                self.keep_failed(id, &file_name(&name, "bin"), &document);
                Err(e)
            }
        };
        self.finish(id, result);
    }

    fn finish(&self, id: i32, result: anyhow::Result<(u64, String)>) {
        let mut jobs = self.jobs.lock().unwrap();
        let Ok(job) = find_job(&mut jobs, id) else {
            return;
        };
        job.completed = Some(self.now());
        job.busy = false;
        match result {
            Ok((receipt, file_name)) => {
                log::info!("ジョブ {id} アップロード完了: {file_name} (receipt id {receipt})");
                job.state = JobState::Completed;
                job.reason = "job-completed-successfully";
                job.message = format!("ファイルボックスにアップロードしました (id {receipt})");
            }
            Err(e) => {
                log::error!("ジョブ {id} 失敗: {e:#}");
                job.state = JobState::Aborted;
                job.reason = "aborted-by-system";
                job.message = format!("{e:#}");
            }
        }
    }

    fn keep_failed(&self, id: i32, file_name: &str, data: &[u8]) {
        if let Some(failed) = &self.failed {
            failed.keep(
                &format!("{}-{id}-{file_name}", Local::now().format("%Y%m%d-%H%M%S")),
                data,
            );
        }
    }

    fn job_id(&self, request: &Message) -> IppResult<i32> {
        if let Some(id) = request.op_int("job-id") {
            return Ok(id);
        }
        // job-uri ends in the job id.
        request
            .op_str("job-uri")
            .and_then(|uri| uri.rsplit('/').next()?.parse().ok())
            .ok_or((status::BAD_REQUEST, "missing job-id".to_string()))
    }

    fn cancel_job(&self, request: &Message) -> IppResult<Message> {
        let id = self.job_id(request)?;
        let mut jobs = self.jobs.lock().unwrap();
        let job = find_job(&mut jobs, id)?;
        if job.state.is_terminal() {
            return Err((
                status::NOT_POSSIBLE,
                format!("job {id} is already finished"),
            ));
        }
        // An upload in flight cannot be taken back.
        if job.busy {
            return Err((
                status::NOT_POSSIBLE,
                format!("job {id} is already being uploaded"),
            ));
        }
        self.cancel(job);
        Ok(Message::response(request, status::OK))
    }

    fn cancel_my_jobs(&self, request: &Message) -> Message {
        let user = request
            .op_str("requesting-user-name")
            .unwrap_or("anonymous");
        let mut jobs = self.jobs.lock().unwrap();
        for job in jobs
            .iter_mut()
            .filter(|j| !j.state.is_terminal() && !j.busy && j.user == user)
        {
            self.cancel(job);
        }
        Message::response(request, status::OK)
    }

    fn cancel(&self, job: &mut Job) {
        job.state = JobState::Canceled;
        job.reason = "job-canceled-by-user";
        job.completed = Some(self.now());
        log::info!("ジョブ {} をキャンセルしました", job.id);
    }

    fn job_response(&self, host: &str, request: &Message, id: i32) -> Message {
        let mut response = Message::response(request, status::OK);
        let mut jobs = self.jobs.lock().unwrap();
        if let Ok(job) = find_job(&mut jobs, id) {
            let wanted = [
                "job-id",
                "job-uri",
                "job-state",
                "job-state-reasons",
                "job-state-message",
            ];
            let attrs = self
                .job_attributes(host, job)
                .into_iter()
                .filter(|a| wanted.contains(&a.name.as_str()));
            response.push_group(tag::JOB, attrs.collect());
        }
        response
    }

    fn get_job_attributes(&self, host: &str, request: &Message) -> IppResult<Message> {
        let id = self.job_id(request)?;
        let mut jobs = self.jobs.lock().unwrap();
        let job = find_job(&mut jobs, id)?;
        let mut response = Message::response(request, status::OK);
        response.push_group(
            tag::JOB,
            filter(self.job_attributes(host, job), requested(request), &[]),
        );
        Ok(response)
    }

    fn get_jobs(&self, host: &str, request: &Message) -> Message {
        let completed = request.op_str("which-jobs") == Some("completed");
        let limit = request
            .op_int("limit")
            .filter(|n| *n > 0)
            .unwrap_or(i32::MAX) as usize;
        let my_jobs = request
            .find(tag::OPERATION, "my-jobs")
            .and_then(|a| a.values.first()?.as_bool())
            == Some(true);
        let user = request
            .op_str("requesting-user-name")
            .unwrap_or("anonymous");
        let wanted =
            requested(request).unwrap_or_else(|| vec!["job-id".to_string(), "job-uri".to_string()]);

        let mut response = Message::response(request, status::OK);
        let jobs = self.jobs.lock().unwrap();
        let ids: Option<Vec<i32>> = request
            .find(tag::OPERATION, "job-ids")
            .map(|a| a.values.iter().filter_map(Value::as_int).collect());
        let selected = jobs
            .iter()
            .filter(|j| match &ids {
                Some(ids) => ids.contains(&j.id),
                None => j.state.is_terminal() == completed,
            })
            .filter(|j| !my_jobs || j.user == user)
            .take(limit);
        for job in selected {
            response.push_group(
                tag::JOB,
                filter(self.job_attributes(host, job), Some(wanted.clone()), &[]),
            );
        }
        response
    }

    fn job_attributes(&self, host: &str, job: &Job) -> Vec<Attr> {
        let time = |stamp: Option<Stamp>| match stamp {
            Some(stamp) => (Value::Integer(stamp.uptime), date_time(stamp.time)),
            None => (ipp::no_value(), ipp::no_value()),
        };
        let (created, created_date) = time(Some(job.created));
        let (processing, processing_date) = time(job.processing);
        let (completed, completed_date) = time(job.completed);
        let pages = if job.state == JobState::Completed {
            job.pages.unwrap_or(0)
        } else {
            0
        };
        vec![
            Attr::new("job-id", Value::Integer(job.id)),
            Attr::new("job-uri", uri(format!("ipp://{host}/ipp/print/{}", job.id))),
            Attr::new("job-uuid", uri(format!("urn:uuid:{}", job.uuid))),
            Attr::new("job-printer-uri", uri(format!("ipp://{host}/ipp/print"))),
            Attr::new("job-name", ipp::name(job.name.clone())),
            Attr::new("job-originating-user-name", ipp::name(job.user.clone())),
            Attr::new("job-state", Value::Enum(job.state as i32)),
            Attr::new("job-state-reasons", keyword(job.reason)),
            Attr::new("job-state-message", text(job.message.clone())),
            Attr::new("job-impressions-completed", Value::Integer(pages)),
            Attr::new("job-media-sheets-completed", Value::Integer(pages)),
            Attr::new("number-of-documents", Value::Integer(1)),
            Attr::new("job-printer-up-time", Value::Integer(self.now().uptime)),
            Attr::new("time-at-creation", created),
            Attr::new("time-at-processing", processing),
            Attr::new("time-at-completed", completed),
            Attr::new("date-time-at-creation", created_date),
            Attr::new("date-time-at-processing", processing_date),
            Attr::new("date-time-at-completed", completed_date),
        ]
    }

    fn get_printer_attributes(&self, host: &str, request: &Message) -> Message {
        let mut response = Message::response(request, status::OK);
        let attrs = filter(
            self.printer_attributes(host),
            requested(request),
            JOB_TEMPLATE,
        );
        response.push_group(tag::PRINTER, attrs);
        response
    }

    fn printer_attributes(&self, host: &str) -> Vec<Attr> {
        let (active, message, changed) = {
            let jobs = self.jobs.lock().unwrap();
            let active = jobs.iter().filter(|j| !j.state.is_terminal()).count() as i32;
            let message = jobs
                .iter()
                .rev()
                .find(|j| j.state.is_terminal())
                .map(|j| j.message.clone());
            // The printer state follows the jobs, so it last changed with one of them.
            let changed = jobs
                .iter()
                .flat_map(|j| [Some(j.created), j.processing, j.completed])
                .flatten()
                .max_by_key(|stamp| stamp.uptime)
                .unwrap_or(Stamp {
                    uptime: 1,
                    time: self.started_at,
                });
            (active, message.unwrap_or_default(), changed)
        };
        let resolution = Value::Resolution(RASTER_DPI, RASTER_DPI, 3);
        let charset = Value::Str(tag::CHARSET, "utf-8".into());
        let language = |s: &str| Value::Str(tag::LANGUAGE, s.into());
        let zero = || Value::Integer(0);
        let company = self
            .freee
            .company()
            .map(|(_, name)| name)
            .unwrap_or_default();

        let mut attrs = vec![
            Attr::new("charset-configured", charset.clone()),
            Attr::new("charset-supported", charset),
            Attr::new("color-supported", Value::Boolean(true)),
            Attr::set(
                "compression-supported",
                keywords(if self.config.accept_gzip {
                    &["none", "gzip"]
                } else {
                    &["none"]
                }),
            ),
            Attr::new("copies-default", Value::Integer(1)),
            Attr::new("copies-supported", Value::Range(1, 1)),
            Attr::new("document-format-default", mime("application/octet-stream")),
            Attr::set(
                "document-format-supported",
                self.formats().into_iter().map(mime).collect(),
            ),
            Attr::new("finishings-default", Value::Enum(3)),
            Attr::new("finishings-supported", Value::Enum(3)),
            Attr::set("generated-natural-language-supported", vec![language("ja")]),
            Attr::new("identify-actions-default", keyword("display")),
            Attr::new("identify-actions-supported", keyword("display")),
            Attr::new("ipp-features-supported", keyword("ipp-everywhere")),
            Attr::set("ipp-versions-supported", keywords(&["1.1", "2.0"])),
            Attr::new("job-ids-supported", Value::Boolean(true)),
            Attr::set(
                "job-creation-attributes-supported",
                keywords(&[
                    "copies",
                    "document-format",
                    "document-name",
                    "job-name",
                    "media",
                    "media-col",
                    "orientation-requested",
                    "print-color-mode",
                    "print-quality",
                    "printer-resolution",
                    "sides",
                ]),
            ),
            Attr::new("media-bottom-margin-supported", zero()),
            Attr::new("media-left-margin-supported", zero()),
            Attr::new("media-right-margin-supported", zero()),
            Attr::new("media-top-margin-supported", zero()),
            Attr::set("media-col-database", MEDIA.iter().map(media_col).collect()),
            Attr::new("media-col-default", media_col(&MEDIA[0])),
            Attr::new("media-col-ready", media_col(&MEDIA[0])),
            Attr::set(
                "media-col-supported",
                keywords(&[
                    "media-bottom-margin",
                    "media-left-margin",
                    "media-right-margin",
                    "media-size",
                    "media-source",
                    "media-top-margin",
                    "media-type",
                ]),
            ),
            Attr::new("media-default", keyword(MEDIA[0].0)),
            Attr::new("media-ready", keyword(MEDIA[0].0)),
            Attr::set(
                "media-supported",
                MEDIA.iter().map(|m| keyword(m.0)).collect(),
            ),
            Attr::set(
                "media-size-supported",
                MEDIA.iter().map(media_size).collect(),
            ),
            Attr::new("media-source-supported", keyword("auto")),
            Attr::new("media-type-supported", keyword("stationery")),
            Attr::new("multiple-document-jobs-supported", Value::Boolean(false)),
            Attr::new("multiple-operation-time-out", Value::Integer(60)),
            Attr::new("multiple-operation-time-out-action", keyword("abort-job")),
            Attr::new("natural-language-configured", language("ja")),
            Attr::set(
                "operations-supported",
                OPERATIONS.iter().map(|o| Value::Enum(*o as i32)).collect(),
            ),
            Attr::new("orientation-requested-default", Value::Enum(3)),
            Attr::set(
                "orientation-requested-supported",
                (3..=6).map(Value::Enum).collect(),
            ),
            Attr::new("output-bin-default", keyword("face-up")),
            Attr::new("output-bin-supported", keyword("face-up")),
            Attr::new("page-ranges-supported", Value::Boolean(false)),
            Attr::new("pages-per-minute", Value::Integer(60)),
            Attr::new("pages-per-minute-color", Value::Integer(60)),
            Attr::new("pdl-override-supported", keyword("attempted")),
            Attr::new("preferred-attributes-supported", Value::Boolean(false)),
            Attr::new("print-color-mode-default", keyword("auto")),
            Attr::set(
                "print-color-mode-supported",
                keywords(&["auto", "color", "monochrome"]),
            ),
            Attr::new("print-content-optimize-default", keyword("auto")),
            Attr::new("print-content-optimize-supported", keyword("auto")),
            Attr::new("print-quality-default", Value::Enum(4)),
            Attr::set(
                "print-quality-supported",
                vec![Value::Enum(4), Value::Enum(5)],
            ),
            Attr::new(
                "printer-device-id",
                text(if self.config.accept_raster {
                    "MFG:freee;MDL:File Box;CMD:PDF,JPEG,PNG,PWGRaster,URF;"
                } else {
                    "MFG:freee;MDL:File Box;CMD:PDF,JPEG,PNG;"
                }),
            ),
            Attr::new(
                "printer-get-attributes-supported",
                keyword("document-format"),
            ),
            Attr::new("printer-geo-location", Value::Other(0x12, Vec::new())),
            Attr::new("printer-icons", uri(format!("http://{host}/icon.png"))),
            Attr::new("printer-info", text(self.name())),
            Attr::new("printer-is-accepting-jobs", Value::Boolean(true)),
            Attr::new("printer-kind", keyword("document")),
            Attr::new("printer-location", text(company.clone())),
            Attr::new("printer-make-and-model", text("freee File Box")),
            Attr::new("printer-more-info", uri(format!("http://{host}/"))),
            Attr::new("printer-name", ipp::name("freee")),
            Attr::new("printer-organization", text(company.clone())),
            Attr::new("printer-organizational-unit", text("")),
            Attr::new("printer-resolution-default", resolution.clone()),
            Attr::new("printer-resolution-supported", resolution.clone()),
            // The configuration never changes while running.
            Attr::new(
                "printer-config-change-date-time",
                date_time(self.started_at),
            ),
            Attr::new("printer-config-change-time", Value::Integer(1)),
            Attr::new("printer-state", Value::Enum(if active > 0 { 4 } else { 3 })),
            Attr::new("printer-state-change-date-time", date_time(changed.time)),
            Attr::new("printer-state-change-time", Value::Integer(changed.uptime)),
            Attr::new("printer-state-reasons", keyword("none")),
            Attr::new("printer-state-message", text(message)),
            Attr::new("printer-up-time", Value::Integer(self.now().uptime)),
            Attr::new("printer-current-time", date_time(Local::now())),
            Attr::new(
                "printer-uri-supported",
                uri(format!("ipp://{host}/ipp/print")),
            ),
            Attr::new("printer-uuid", uri(format!("urn:uuid:{}", self.uuid))),
            Attr::new("queued-job-count", Value::Integer(active)),
            Attr::new("sides-default", keyword("one-sided")),
            Attr::new("sides-supported", keyword("one-sided")),
            Attr::new("uri-authentication-supported", keyword("none")),
            Attr::new("uri-security-supported", keyword("none")),
            Attr::set(
                "which-jobs-supported",
                keywords(&["completed", "not-completed"]),
            ),
        ];
        if self.config.accept_raster {
            attrs.extend([
                Attr::new(
                    "pwg-raster-document-resolution-supported",
                    resolution.clone(),
                ),
                Attr::new("pwg-raster-document-sheet-back", keyword("normal")),
                Attr::set(
                    "pwg-raster-document-type-supported",
                    keywords(&["sgray_8", "srgb_8"]),
                ),
                Attr::set("urf-supported", keywords(URF_CAPABILITIES)),
            ]);
        }
        attrs
    }
}

pub const URF_CAPABILITIES: &[&str] = &["V1.4", "CP1", "PQ4-5", "RS300", "SRGB24", "W8"];

/// Attributes that belong to the "job-template" group; everything else is
/// "printer-description".
const JOB_TEMPLATE: &[&str] = &[
    "copies-default",
    "copies-supported",
    "media-bottom-margin-supported",
    "media-col-database",
    "media-col-default",
    "media-col-ready",
    "media-col-supported",
    "media-default",
    "media-left-margin-supported",
    "media-ready",
    "media-right-margin-supported",
    "media-size-supported",
    "media-source-supported",
    "media-supported",
    "media-top-margin-supported",
    "media-type-supported",
    "orientation-requested-default",
    "orientation-requested-supported",
    "output-bin-default",
    "output-bin-supported",
    "page-ranges-supported",
    "print-color-mode-default",
    "print-color-mode-supported",
    "print-content-optimize-default",
    "print-content-optimize-supported",
    "print-quality-default",
    "print-quality-supported",
    "printer-resolution-default",
    "printer-resolution-supported",
    "sides-default",
    "sides-supported",
];

type IppResult<T> = std::result::Result<T, (u16, String)>;

fn find_job(jobs: &mut [Job], id: i32) -> IppResult<&mut Job> {
    jobs.iter_mut()
        .find(|j| j.id == id)
        .ok_or((status::NOT_FOUND, format!("job {id} not found")))
}

fn requested(request: &Message) -> Option<Vec<String>> {
    let attr = request.find(tag::OPERATION, "requested-attributes")?;
    Some(
        attr.values
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
    )
}

/// Applies requested-attributes, including the group names.
fn filter(attrs: Vec<Attr>, requested: Option<Vec<String>>, job_template: &[&str]) -> Vec<Attr> {
    let Some(requested) = requested else {
        return attrs;
    };
    let has = |name: &str| requested.iter().any(|r| r == name);
    if has("all") {
        return attrs;
    }
    let (template, description) = (
        has("job-template"),
        has("printer-description") || has("job-description"),
    );
    attrs
        .into_iter()
        .filter(|a| {
            let name = a.name.as_str();
            has(name)
                || if job_template.contains(&name) {
                    template
                } else {
                    description
                }
        })
        .collect()
}

fn media_size(media: &(&str, i32, i32)) -> Value {
    Value::Collection(vec![
        Attr::new("x-dimension", Value::Integer(media.1)),
        Attr::new("y-dimension", Value::Integer(media.2)),
    ])
}

fn media_col(media: &(&str, i32, i32)) -> Value {
    Value::Collection(vec![
        Attr::new("media-size", media_size(media)),
        Attr::new("media-bottom-margin", Value::Integer(0)),
        Attr::new("media-left-margin", Value::Integer(0)),
        Attr::new("media-right-margin", Value::Integer(0)),
        Attr::new("media-top-margin", Value::Integer(0)),
        Attr::new("media-source", keyword("auto")),
        Attr::new("media-type", keyword("stationery")),
    ])
}

/// RFC 2579 DateAndTime.
fn date_time(time: DateTime<Local>) -> Value {
    let offset = time.offset().local_minus_utc();
    let year = (time.year() as u16).to_be_bytes();
    Value::DateTime([
        year[0],
        year[1],
        time.month() as u8,
        time.day() as u8,
        time.hour() as u8,
        time.minute() as u8,
        time.second().min(59) as u8,
        0,
        if offset < 0 { b'-' } else { b'+' },
        (offset.abs() / 3600) as u8,
        (offset.abs() % 3600 / 60) as u8,
    ])
}

/// Turns the received document into the file to upload.
/// Returns the upload and, when known, its page count. On failure the
/// document comes back so that the caller can keep it.
pub fn prepare(
    format: Option<&str>,
    gzip: bool,
    allow_raster: bool,
    document: Vec<u8>,
    job_name: &str,
) -> Result<(Upload, Option<i32>), (anyhow::Error, Vec<u8>)> {
    let document = if gzip {
        let mut unpacked = Vec::new();
        match flate2::read::GzDecoder::new(&document[..])
            .read_to_end(&mut unpacked)
            .context("gzip を展開できません")
        {
            Ok(_) => unpacked,
            Err(e) => return Err((e, document)),
        }
    } else {
        document
    };

    // Clients often send application/octet-stream, so trust the content first.
    let sniffed = if document.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if document.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if document.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if raster::is_pwg(&document) {
        Some("image/pwg-raster")
    } else if raster::is_urf(&document) {
        Some("image/urf")
    } else {
        None
    };
    let format = sniffed.or(format).unwrap_or("application/octet-stream");

    let (data, mime, extension, pages) = match format {
        "application/pdf" => (document, "application/pdf", "pdf", None),
        "image/jpeg" => (document, "image/jpeg", "jpg", Some(1)),
        "image/png" => (document, "image/png", "png", Some(1)),
        "image/pwg-raster" | "image/urf" if allow_raster => match raster::to_pdf(&document) {
            Ok((pdf, pages)) => (pdf, "application/pdf", "pdf", Some(pages as i32)),
            Err(e) => return Err((e, document)),
        },
        other => return Err((anyhow!("対応していない文書形式です: {other}"), document)),
    };
    let upload = Upload {
        file_name: file_name(job_name, extension),
        mime,
        data,
        description: job_name.chars().take(255).collect(),
        document_type: None,
    };
    Ok((upload, pages))
}

/// Builds an upload file name from the job name.
fn file_name(job_name: &str, extension: &str) -> String {
    let cleaned: String = job_name
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut stem = cleaned.trim().to_string();
    // Drop an extension the application already put into the title.
    for known in [".pdf", ".jpg", ".jpeg", ".png"] {
        let cut = stem.len().saturating_sub(known.len());
        if stem.is_char_boundary(cut) && stem[cut..].eq_ignore_ascii_case(known) {
            stem.truncate(cut);
            break;
        }
    }
    let mut stem: String = stem.trim().trim_matches('.').chars().take(100).collect();
    if stem.is_empty() {
        stem = format!("print-{}", Local::now().format("%Y%m%d-%H%M%S"));
    }
    format!("{stem}.{extension}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(file_name("請求書 2026/09", "pdf"), "請求書 2026_09.pdf");
        assert_eq!(file_name("invoice.PDF", "pdf"), "invoice.pdf");
        assert_eq!(file_name("a\"b\r\nc", "pdf"), "a_b__c.pdf");
        assert!(file_name("", "pdf").starts_with("print-"));
        assert!(file_name(" .pdf ", "jpg").starts_with("print-"));
    }

    #[test]
    fn prepare_sniffs_the_format() {
        let (upload, pages) = prepare(
            Some("application/octet-stream"),
            false,
            true,
            b"%PDF-1.7\n".to_vec(),
            "doc",
        )
        .ok()
        .unwrap();
        assert_eq!(
            (upload.mime, upload.file_name.as_str(), pages),
            ("application/pdf", "doc.pdf", None)
        );
        let (_, returned) = prepare(None, false, true, b"hello".to_vec(), "doc")
            .err()
            .unwrap();
        assert_eq!(returned, b"hello");
        assert!(prepare(None, false, false, b"RaS2".to_vec(), "doc").is_err());
    }

    #[test]
    fn filters_requested_attributes() {
        let attrs = || {
            vec![
                Attr::new("media-default", keyword("a")),
                Attr::new("printer-name", keyword("b")),
            ]
        };
        let names = |requested: &[&str]| -> Vec<String> {
            let requested = requested.iter().map(|s| s.to_string()).collect();
            filter(attrs(), Some(requested), JOB_TEMPLATE)
                .into_iter()
                .map(|a| a.name)
                .collect()
        };
        assert_eq!(names(&["all"]).len(), 2);
        assert_eq!(names(&["job-template"]), ["media-default"]);
        assert_eq!(names(&["printer-description"]), ["printer-name"]);
        assert_eq!(names(&["printer-name", "unknown"]), ["printer-name"]);
    }
}
