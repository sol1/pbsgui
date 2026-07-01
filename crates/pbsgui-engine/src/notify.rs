//! Notifications: tell someone when a job finishes.
//!
//! A single global [`NotificationSettings`] (in `config_dir/notifications.json`)
//! plus two channels: email over SMTP and a Slack-compatible webhook. Secrets
//! (the SMTP password and the webhook URL) live in the OS credential store under
//! `notify:smtp` / `notify:webhook`, never in the config file. Sending is
//! best-effort: a channel failure is logged, never fatal to the backup.

use std::fmt::Write;

use anyhow::Context;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use pbs_client::session::BackupStats;
use pbsgui_ipc::{EmailSecurity, EmailSettings, NotificationSettings, NotifyChannel};

use crate::config::config_dir;
use crate::secrets;

const SMTP_SECRET_KEY: &str = "notify:smtp";
const WEBHOOK_SECRET_KEY: &str = "notify:webhook";

fn config_path() -> std::path::PathBuf {
    config_dir().join("notifications.json")
}

/// The default settings: nothing enabled, but failure alerts on once a channel is.
fn default_settings() -> NotificationSettings {
    NotificationSettings {
        on_success: false,
        on_failure: true,
        on_stall: true,
        email: EmailSettings {
            enabled: false,
            host: String::new(),
            port: 587,
            security: EmailSecurity::Starttls,
            username: String::new(),
            from: String::new(),
            to: Vec::new(),
        },
        webhook: pbsgui_ipc::WebhookSettings { enabled: false },
    }
}

/// Load the global settings (defaults if none saved yet).
pub fn load() -> NotificationSettings {
    std::fs::read(config_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(default_settings)
}

/// Persist the global settings.
pub fn save(settings: &NotificationSettings) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(config_path(), serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}

/// Whether each secret is currently stored (so the UI can show "set").
pub fn secret_flags() -> (bool, bool) {
    let has_smtp = secrets::get(SMTP_SECRET_KEY).ok().flatten().is_some();
    let has_webhook = secrets::get(WEBHOOK_SECRET_KEY).ok().flatten().is_some();
    (has_smtp, has_webhook)
}

/// Store the SMTP password (skip when `None`, so a save keeps the existing one).
pub fn set_smtp_password(password: Option<&str>) -> anyhow::Result<()> {
    if let Some(p) = password {
        secrets::set(SMTP_SECRET_KEY, p)?;
    }
    Ok(())
}

/// Store the webhook URL (skip when `None`).
pub fn set_webhook_url(url: Option<&str>) -> anyhow::Result<()> {
    if let Some(u) = url {
        secrets::set(WEBHOOK_SECRET_KEY, u)?;
    }
    Ok(())
}

/// One job's outcome, the input to a notification.
pub struct JobOutcome<'a> {
    pub job_name: &'a str,
    /// What ran, e.g. "full backup", "log backup", "file backup".
    pub kind: &'a str,
    /// Databases backed up (empty for a file job).
    pub databases: &'a [String],
    pub success: bool,
    /// "ok", "no-change", or "error".
    pub status: &'a str,
    pub message: &'a str,
    pub stats: Option<&'a BackupStats>,
}

/// A one-line headline, e.g. "nightly: log backup of Sales, HR succeeded".
fn headline(o: &JobOutcome<'_>) -> String {
    let verb = if o.success { "succeeded" } else { "FAILED" };
    let of = if o.databases.is_empty() {
        String::new()
    } else {
        format!(" of {}", o.databases.join(", "))
    };
    format!("{}: {}{} {}", o.job_name, o.kind, of, verb)
}

/// Notify about a finished job through every enabled channel, honoring the
/// success/failure triggers. Best-effort: logs and swallows channel errors.
pub async fn job_finished(outcome: JobOutcome<'_>) {
    let settings = load();
    let wanted = if outcome.success {
        settings.on_success
    } else {
        settings.on_failure
    };
    if !wanted {
        return;
    }

    let subject = format!("[pbsgui] {}", headline(&outcome));
    let body = render_body(&outcome);

    if settings.email.enabled {
        if let Err(e) = send_email(&settings.email, &subject, &body).await {
            tracing::warn!("email notification failed: {e:#}");
        }
    }
    if settings.webhook.enabled {
        if let Err(e) = send_webhook(&outcome).await {
            tracing::warn!("webhook notification failed: {e:#}");
        }
    }
}

/// Send a test message through one channel, returning the outcome to report.
pub async fn send_test(channel: NotifyChannel) -> anyhow::Result<()> {
    let settings = load();
    let subject = "[pbsgui] test notification".to_string();
    let body = "This is a test notification from pbsgui. If you can read this, the channel works."
        .to_string();
    let outcome = JobOutcome {
        job_name: "test",
        kind: "test notification",
        databases: &[],
        success: true,
        status: "ok",
        message: &body,
        stats: None,
    };
    match channel {
        NotifyChannel::Email => send_email(&settings.email, &subject, &body).await,
        NotifyChannel::Webhook => send_webhook(&outcome).await,
    }
}

/// Warn that a point-in-time backup chain has stalled (no snapshot has reached
/// its PBS group within the expected window). Honors `on_stall` and the enabled
/// channels. Self-contained: no external service is contacted.
pub async fn backup_stalled(job_name: &str, database: &str, hours: i64) {
    let settings = load();
    if !settings.on_stall {
        return;
    }
    let subject = format!("[pbsgui] {job_name}: backup chain stalled");
    let body = format!(
        "No backup has reached the chain for database {database} in about {hours}h. \
Point-in-time recovery is falling behind and the transaction log may be growing. \
Check that pbsgui is running on the active (Always On preferred) replica and that \
SQL Server is reachable."
    );
    if settings.email.enabled {
        if let Err(e) = send_email(&settings.email, &subject, &body).await {
            tracing::warn!("stall email notification failed: {e:#}");
        }
    }
    if settings.webhook.enabled {
        let dbs = [database.to_string()];
        let outcome = JobOutcome {
            job_name,
            kind: "point-in-time backup",
            databases: &dbs,
            success: false,
            status: "stalled",
            message: &body,
            stats: None,
        };
        if let Err(e) = send_webhook(&outcome).await {
            tracing::warn!("stall webhook notification failed: {e:#}");
        }
    }
}

/// The at-a-glance status symbol and word for a headline. `no-change` shares the
/// warning symbol with a stall: nothing was backed up, which may be expected
/// (unchanged source, non-preferred replica) or may want a second look.
fn status_badge(o: &JobOutcome<'_>) -> (&'static str, &'static str) {
    match o.status {
        "ok" => ("\u{2705}", "OK"),                       // green tick
        "no-change" => ("\u{26a0}\u{fe0f}", "NO CHANGE"), // warning
        "error" => ("\u{274c}", "FAILED"),                // cross
        "stalled" => ("\u{26a0}\u{fe0f}", "STALLED"),     // warning
        _ if o.success => ("\u{2705}", "OK"),
        _ => ("\u{274c}", "FAILED"),
    }
}

/// The databases for a headline, truncated so a job over many databases stays to
/// one short line.
fn db_list(dbs: &[String]) -> String {
    const MAX: usize = 3;
    if dbs.len() <= MAX {
        dbs.join(", ")
    } else {
        format!("{} (+{} more)", dbs[..MAX].join(", "), dbs.len() - MAX)
    }
}

/// Trim `s` to at most `max` characters (on a char boundary), adding an ellipsis
/// when cut, so a long error or message does not sprawl across the channel.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{}\u{2026}", head.trim_end())
    }
}

/// The one-line figures for a completed backup: total size, new data written, and
/// dedup rate, all human readable.
fn stats_line(s: &BackupStats) -> String {
    let sep = " \u{00b7} ";
    let mut line = format!("{} backed up", crate::backup::human_bytes(s.bytes));
    if s.stored_bytes > 0 {
        let _ = write!(
            line,
            "{sep}{} stored",
            crate::backup::human_bytes(s.stored_bytes)
        );
    }
    if s.chunks > 0 {
        let dedup = s.reused as f64 / s.chunks as f64 * 100.0;
        let _ = write!(line, "{sep}{dedup:.0}% dedup");
    }
    line
}

/// A compact, at-a-glance message for a Slack-compatible webhook: status first
/// (symbol plus word), then what ran, then either the figures (for a backup) or a
/// short reason (for a skip, failure, or stall). One or two lines.
fn slack_text(o: &JobOutcome<'_>) -> String {
    let (icon, word) = status_badge(o);
    let sep = " \u{00b7} ";
    let mut head = format!("{icon} *{word}*{sep}{}", o.job_name);
    if o.databases.is_empty() {
        let _ = write!(head, "{sep}{}", o.kind);
    } else {
        let _ = write!(head, "{sep}{} of {}", o.kind, db_list(o.databases));
    }
    let detail = match o.stats {
        Some(s) => stats_line(s),
        None => truncate(o.message, 200),
    };
    if detail.is_empty() {
        head
    } else {
        format!("{head}\n{detail}")
    }
}

fn render_body(o: &JobOutcome<'_>) -> String {
    let mut lines = vec![format!("Job: {}", o.job_name), format!("Type: {}", o.kind)];
    if !o.databases.is_empty() {
        lines.push(format!("Databases: {}", o.databases.join(", ")));
    }
    lines.push(format!("Status: {}", o.status));
    // The message already carries the byte/chunk figures for a backup; the webhook
    // payload also includes the structured `stats` object for machine consumers.
    lines.push(format!("Message: {}", o.message));
    lines.join("\n")
}

async fn send_email(email: &EmailSettings, subject: &str, body: &str) -> anyhow::Result<()> {
    if email.host.trim().is_empty() {
        anyhow::bail!("no SMTP host configured");
    }
    if email.to.is_empty() {
        anyhow::bail!("no recipients configured");
    }

    let mut builder = Message::builder()
        .from(
            email
                .from
                .parse()
                .with_context(|| format!("invalid from address: {}", email.from))?,
        )
        .subject(subject);
    for to in &email.to {
        builder = builder.to(to
            .parse()
            .with_context(|| format!("invalid recipient: {to}"))?);
    }
    let message = builder.body(body.to_string())?;

    let tls = TlsParameters::new(email.host.clone())?;
    let mut transport =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&email.host).port(email.port);
    transport = match email.security {
        EmailSecurity::Tls => transport.tls(Tls::Wrapper(tls)),
        EmailSecurity::Starttls => transport.tls(Tls::Required(tls)),
        EmailSecurity::None => transport,
    };
    if !email.username.is_empty() {
        let password = secrets::get(SMTP_SECRET_KEY)?
            .ok_or_else(|| anyhow::anyhow!("no SMTP password stored"))?;
        transport = transport.credentials(Credentials::new(email.username.clone(), password));
    }

    transport.build().send(message).await.context("SMTP send")?;
    Ok(())
}

async fn send_webhook(o: &JobOutcome<'_>) -> anyhow::Result<()> {
    let url = secrets::get(WEBHOOK_SECRET_KEY)?
        .ok_or_else(|| anyhow::anyhow!("no webhook URL stored"))?;

    // `text` is the compact human line a Slack incoming webhook renders; the
    // structured fields below are for generic machine consumers.
    let mut payload = serde_json::json!({
        "text": slack_text(o),
        "job": o.job_name,
        "kind": o.kind,
        "databases": o.databases,
        "status": o.status,
        "success": o.success,
        "message": o.message,
    });
    if let Some(s) = o.stats {
        payload["stats"] = serde_json::json!({
            "bytes": s.bytes,
            "chunks": s.chunks,
            "uploaded": s.uploaded,
            "reused": s.reused,
        });
    }

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("posting to the webhook")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("webhook returned HTTP {}", status.as_u16());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(bytes: u64, stored: u64, chunks: u64, reused: u64) -> BackupStats {
        BackupStats {
            chunks,
            reused,
            uploaded: chunks - reused,
            bytes,
            uploaded_bytes: 0,
            stored_bytes: stored,
            csum: [0u8; 32],
        }
    }

    fn outcome<'a>(
        status: &'a str,
        success: bool,
        dbs: &'a [String],
        message: &'a str,
        stats: Option<&'a BackupStats>,
    ) -> JobOutcome<'a> {
        JobOutcome {
            job_name: "Nightly backup VIP DB",
            kind: "full backup",
            databases: dbs,
            success,
            status,
            message,
            stats,
        }
    }

    #[test]
    fn success_is_two_compact_lines_with_human_bytes() {
        let dbs = ["GES_Live".to_string()];
        // The raw byte count from the original report.
        let s = stats(768706259456, 9816442012, 131476, 128211);
        let text = slack_text(&outcome(
            "ok",
            true,
            &dbs,
            "backed up 1 database(s)",
            Some(&s),
        ));

        let (head, detail) = text.split_once('\n').expect("two lines");
        assert!(head.starts_with("\u{2705} *OK* \u{00b7} Nightly backup VIP DB"));
        assert!(head.contains("full backup of GES_Live"));
        // Human readable, not raw bytes.
        assert!(detail.contains("GiB"), "{detail}");
        assert!(
            detail.contains("backed up") && detail.contains("stored") && detail.contains("dedup")
        );
        assert!(!text.contains("768706259456"));
        // No leftover chunk-count noise in the compact line.
        assert!(!detail.contains("131476"));
    }

    #[test]
    fn no_change_uses_the_warning_badge() {
        let text = slack_text(&outcome(
            "no-change",
            true,
            &[],
            "no changes since last run; skipped",
            None,
        ));
        assert!(text.starts_with("\u{26a0}\u{fe0f} *NO CHANGE*"));
        assert!(text.contains("no changes since last run"));
    }

    #[test]
    fn failure_leads_with_the_cross_and_shows_the_reason() {
        let dbs = ["GES_Live".to_string()];
        let text = slack_text(&outcome(
            "error",
            false,
            &dbs,
            "BACKUP failed: login timeout",
            None,
        ));
        assert!(text.starts_with("\u{274c} *FAILED* \u{00b7} Nightly backup VIP DB"));
        assert!(text.contains("BACKUP failed: login timeout"));
    }

    #[test]
    fn many_databases_are_truncated() {
        let dbs: Vec<String> = (1..=7).map(|i| format!("DB{i}")).collect();
        let s = stats(1024, 512, 2, 1);
        let text = slack_text(&outcome("ok", true, &dbs, "", Some(&s)));
        assert!(text.contains("DB1, DB2, DB3 (+4 more)"), "{text}");
    }

    #[test]
    fn long_messages_are_trimmed() {
        let long = "x".repeat(500);
        let text = slack_text(&outcome("error", false, &[], &long, None));
        assert!(text.chars().count() < 260);
        assert!(text.ends_with('\u{2026}'));
    }
}
