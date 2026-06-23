//! Optional Prometheus metrics exporter.
//!
//! The engine is a long-running service, so it holds the last result of every job
//! and exposes it on demand: an exporter, not a Pushgateway. Two transports share
//! one renderer (`render`): `endpoint` serves `GET /metrics` over a tiny HTTP
//! listener for Prometheus to scrape, and `textfile` writes
//! `<dir>/pbsgui.prom` for a node/windows_exporter textfile collector. Off by
//! default. Metrics never include secrets (no repositories, tokens, keys, or
//! server hostnames) - only job names and database names as labels.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

use pbs_client::session::BackupStats;
use pbsgui_ipc::{Job, JobDestination, JobSource, MetricsMode, MetricsSettings, SqlProtection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

use crate::config::{config_dir, unix_now};
use crate::jobstore::JobStore;

fn config_path() -> std::path::PathBuf {
    config_dir().join("metrics.json")
}

/// Default exporter settings: not exported.
fn default_settings() -> MetricsSettings {
    MetricsSettings {
        mode: MetricsMode::Off,
        port: 9654,
        bind: "127.0.0.1".to_string(),
        textfile_dir: String::new(),
    }
}

pub fn load() -> MetricsSettings {
    std::fs::read(config_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(default_settings)
}

pub fn save(settings: &MetricsSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(config_path(), serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}

/// Outcome of a run, for the per-result counters.
#[derive(Clone, Copy)]
pub enum RunResult {
    Success,
    Skipped,
    Failure,
}

/// Point-in-time chain status for one database, gathered by the health check.
#[derive(Clone)]
pub struct SqlDbStatus {
    pub database: String,
    /// Newest snapshot time across the full and log groups (unix seconds).
    pub chain_latest: Option<i64>,
    pub stalled: bool,
    pub full_count: u32,
    pub log_count: u32,
    /// Span of the recoverable window (latest minus earliest full), seconds.
    pub pit_window_secs: Option<i64>,
}

#[derive(Default, Clone)]
struct JobRuntime {
    last_run: Option<i64>,
    last_success: Option<i64>,
    last_ok: Option<bool>,
    last_duration: Option<f64>,
    last_bytes: Option<u64>,
    last_chunks: Option<u64>,
    last_uploaded: Option<u64>,
    last_reused: Option<u64>,
    runs_success: u64,
    runs_failure: u64,
    runs_skipped: u64,
    bytes_total: u64,
    running: bool,
    databases: Vec<SqlDbStatus>,
}

struct State {
    start_time: i64,
    jobs: HashMap<String, JobRuntime>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            start_time: unix_now(),
            jobs: HashMap::new(),
        })
    })
}

fn server_handle() -> &'static Mutex<Option<AbortHandle>> {
    static H: OnceLock<Mutex<Option<AbortHandle>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

/// Mark a job as currently running (or not), for `pbsgui_job_running`.
pub fn set_running(job_id: &str, running: bool) {
    let mut st = state().lock().unwrap();
    st.jobs.entry(job_id.to_string()).or_default().running = running;
}

/// Record a finished run's outcome and last-run gauges.
pub fn record_run(
    job_id: &str,
    result: RunResult,
    duration_secs: f64,
    stats: Option<&BackupStats>,
) {
    let now = unix_now();
    let mut st = state().lock().unwrap();
    let j = st.jobs.entry(job_id.to_string()).or_default();
    j.running = false;
    j.last_run = Some(now);
    j.last_duration = Some(duration_secs);
    match result {
        RunResult::Success => {
            j.runs_success += 1;
            j.last_ok = Some(true);
            j.last_success = Some(now);
        }
        // A skip (no changes, or not the preferred replica) is not a failure.
        RunResult::Skipped => {
            j.runs_skipped += 1;
            j.last_ok = Some(true);
        }
        RunResult::Failure => {
            j.runs_failure += 1;
            j.last_ok = Some(false);
        }
    }
    if let Some(s) = stats {
        j.last_bytes = Some(s.bytes);
        j.last_chunks = Some(s.chunks);
        j.last_uploaded = Some(s.uploaded);
        j.last_reused = Some(s.reused);
        j.bytes_total += s.bytes;
    }
}

/// Replace a job's per-database point-in-time status (from the health check).
pub fn set_sql_status(job_id: &str, databases: Vec<SqlDbStatus>) {
    let mut st = state().lock().unwrap();
    st.jobs.entry(job_id.to_string()).or_default().databases = databases;
}

/// Escape a Prometheus label value (`\`, `"`, and newlines).
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn help(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Render the Prometheus exposition text for all jobs. Cheap: reads in-memory
/// state only (the SQL chain figures are refreshed by the health check, cached).
pub fn render(store: &JobStore) -> String {
    let st = state().lock().unwrap();
    let jobs = store.list();
    let mut out = String::new();

    help(&mut out, "pbsgui_build_info", "Build information.", "gauge");
    let _ = writeln!(
        out,
        "pbsgui_build_info{{version=\"{}\",commit=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION"),
        esc(option_env!("PBSGUI_BUILD").unwrap_or("dev"))
    );
    help(
        &mut out,
        "pbsgui_jobs",
        "Number of configured jobs.",
        "gauge",
    );
    let _ = writeln!(out, "pbsgui_jobs {}", jobs.len());
    help(
        &mut out,
        "pbsgui_engine_start_timestamp_seconds",
        "Engine start time.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "pbsgui_engine_start_timestamp_seconds {}",
        st.start_time
    );

    // Per-job metric families, grouped so each HELP/TYPE appears once.
    help(&mut out, "pbsgui_job_info", "Static job metadata.", "gauge");
    for job in &jobs {
        let (source, plan) = match &job.source {
            JobSource::Sql { protection, .. } => ("sql", protection_label(protection)),
            JobSource::Files { .. } => ("files", ""),
        };
        let destination = match &job.destination {
            JobDestination::Pbs { .. } => "pbs",
            JobDestination::Folder { .. } => "folder",
        };
        let _ = writeln!(
            out,
            "pbsgui_job_info{{job=\"{}\",job_id=\"{}\",source=\"{source}\",destination=\"{destination}\",plan=\"{plan}\",encrypted=\"{}\"}} 1",
            esc(&job.name),
            esc(&job.id),
            job.encrypted
        );
    }

    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_running",
        "1 while a backup for the job is running.",
        |r| Some(if r.running { 1.0 } else { 0.0 }),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_run_timestamp_seconds",
        "Time of the last run.",
        |r| r.last_run.map(|v| v as f64),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_success_timestamp_seconds",
        "Time of the last successful run.",
        |r| r.last_success.map(|v| v as f64),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_run_success",
        "1 if the last run succeeded, else 0.",
        |r| r.last_ok.map(|ok| if ok { 1.0 } else { 0.0 }),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_duration_seconds",
        "Duration of the last run.",
        |r| r.last_duration,
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_size_bytes",
        "Size of the last backup.",
        |r| r.last_bytes.map(|v| v as f64),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_chunks",
        "Chunks in the last backup.",
        |r| r.last_chunks.map(|v| v as f64),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_chunks_uploaded",
        "Chunks uploaded in the last backup.",
        |r| r.last_uploaded.map(|v| v as f64),
    );
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_last_chunks_reused",
        "Chunks reused (deduplicated) in the last backup.",
        |r| r.last_reused.map(|v| v as f64),
    );

    // Counters.
    help(
        &mut out,
        "pbsgui_job_runs_total",
        "Runs by result since engine start.",
        "counter",
    );
    for job in &jobs {
        if let Some(r) = st.jobs.get(&job.id) {
            let n = esc(&job.name);
            let _ = writeln!(
                out,
                "pbsgui_job_runs_total{{job=\"{n}\",result=\"success\"}} {}",
                r.runs_success
            );
            let _ = writeln!(
                out,
                "pbsgui_job_runs_total{{job=\"{n}\",result=\"failure\"}} {}",
                r.runs_failure
            );
            let _ = writeln!(
                out,
                "pbsgui_job_runs_total{{job=\"{n}\",result=\"skipped\"}} {}",
                r.runs_skipped
            );
        }
    }
    job_gauge(
        &mut out,
        &st,
        &jobs,
        "pbsgui_job_bytes_backed_up_total",
        "Cumulative bytes backed up since engine start.",
        |r| Some(r.bytes_total as f64),
    );

    // SQL point-in-time, per database.
    let sql_help = [
        (
            "pbsgui_sql_chain_latest_timestamp_seconds",
            "Newest snapshot time in the shared PBS group.",
            "gauge",
        ),
        (
            "pbsgui_sql_chain_stalled",
            "1 if the point-in-time chain has stopped advancing.",
            "gauge",
        ),
        (
            "pbsgui_sql_full_count",
            "Full backups retained in the group.",
            "gauge",
        ),
        (
            "pbsgui_sql_log_count",
            "Log backups retained in the group.",
            "gauge",
        ),
        (
            "pbsgui_sql_pit_window_seconds",
            "Span of the recoverable point-in-time window.",
            "gauge",
        ),
    ];
    for (name, h, kind) in sql_help {
        help(&mut out, name, h, kind);
        for job in &jobs {
            let Some(r) = st.jobs.get(&job.id) else {
                continue;
            };
            for db in &r.databases {
                let labels = format!(
                    "job=\"{}\",database=\"{}\"",
                    esc(&job.name),
                    esc(&db.database)
                );
                let val = match name {
                    "pbsgui_sql_chain_latest_timestamp_seconds" => {
                        db.chain_latest.map(|v| v as f64)
                    }
                    "pbsgui_sql_chain_stalled" => Some(if db.stalled { 1.0 } else { 0.0 }),
                    "pbsgui_sql_full_count" => Some(db.full_count as f64),
                    "pbsgui_sql_log_count" => Some(db.log_count as f64),
                    "pbsgui_sql_pit_window_seconds" => db.pit_window_secs.map(|v| v as f64),
                    _ => None,
                };
                if let Some(v) = val {
                    let _ = writeln!(out, "{name}{{{labels}}} {}", fmt(v));
                }
            }
        }
    }

    out
}

/// Emit one gauge family across all jobs, skipping jobs with no value yet.
fn job_gauge(
    out: &mut String,
    st: &State,
    jobs: &[Job],
    name: &str,
    help_text: &str,
    pick: impl Fn(&JobRuntime) -> Option<f64>,
) {
    help(out, name, help_text, "gauge");
    for job in jobs {
        if let Some(r) = st.jobs.get(&job.id) {
            if let Some(v) = pick(r) {
                let _ = writeln!(out, "{name}{{job=\"{}\"}} {}", esc(&job.name), fmt(v));
            }
        }
    }
}

/// Format a value: integers without a decimal point, otherwise plain.
fn fmt(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn protection_label(p: &SqlProtection) -> &'static str {
    match p {
        SqlProtection::PointInTime { .. } => "point_in_time",
        SqlProtection::DailyRestorePoints { .. } => "daily_restore_points",
        SqlProtection::SecondaryCopy { .. } => "secondary_copy",
    }
}

/// Write the textfile if the current mode is `textfile`. Atomic (tmp then rename).
pub fn write_textfile(store: &JobStore) {
    let settings = load();
    if settings.mode != MetricsMode::Textfile {
        return;
    }
    let dir = settings.textfile_dir.trim();
    if dir.is_empty() {
        return;
    }
    let body = render(store);
    let final_path = std::path::Path::new(dir).join("pbsgui.prom");
    let tmp = std::path::Path::new(dir).join("pbsgui.prom.tmp");
    if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &final_path)) {
        tracing::warn!("writing the metrics textfile failed: {e:#}");
    }
}

/// Apply the saved settings: stop any running endpoint, then start the chosen
/// transport. Call at engine startup and whenever the settings are saved.
pub fn apply(store: std::sync::Arc<JobStore>) {
    if let Some(handle) = server_handle().lock().unwrap().take() {
        handle.abort();
    }
    let settings = load();
    match settings.mode {
        MetricsMode::Endpoint => {
            let task = tokio::spawn(serve(settings.bind, settings.port, store));
            *server_handle().lock().unwrap() = Some(task.abort_handle());
        }
        MetricsMode::Textfile => write_textfile(&store),
        MetricsMode::Off => {}
    }
}

/// A minimal HTTP/1.1 listener serving `GET /metrics`. The endpoint is
/// unauthenticated; binding it beyond loopback exposes the (secret-free) metrics
/// to the network, so a non-loopback bind is warned about loudly.
async fn serve(bind: String, port: u16, store: std::sync::Arc<JobStore>) {
    match bind.parse::<std::net::IpAddr>() {
        Ok(ip) if !ip.is_loopback() => tracing::warn!(
            %bind,
            "metrics endpoint is bound beyond localhost and is UNAUTHENTICATED; \
             restrict it with a firewall"
        ),
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(%bind, "metrics bind address is not a valid IP; not starting");
            return;
        }
    }
    let listener = match TcpListener::bind((bind.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("metrics endpoint cannot bind {bind}:{port}: {e:#}");
            return;
        }
    };
    tracing::info!(%bind, port, "metrics endpoint listening on /metrics");
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let store = store.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = match sock.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let mut tokens = req.split_whitespace();
            let method = tokens.next().unwrap_or("");
            let path = tokens.next().unwrap_or("");
            let (status, ctype, body) = if method != "GET" {
                (
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "method not allowed\n".to_string(),
                )
            } else if path.starts_with("/metrics") {
                (
                    "200 OK",
                    "text/plain; version=0.0.4; charset=utf-8",
                    render(&store),
                )
            } else if path == "/" {
                (
                    "200 OK",
                    "text/html; charset=utf-8",
                    "<html><body>pbsgui metrics: <a href=\"/metrics\">/metrics</a></body></html>"
                        .to_string(),
                )
            } else {
                (
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    "not found\n".to_string(),
                )
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
