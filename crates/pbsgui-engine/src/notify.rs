//! Notifications: tell someone when a job finishes.
//!
//! A single global [`NotificationSettings`] (in `config_dir/notifications.json`)
//! plus two channels: email over SMTP and a Slack-compatible webhook. Secrets
//! (the SMTP password and the webhook URL) live in the OS credential store under
//! `notify:smtp` / `notify:webhook`, never in the config file. Sending is
//! best-effort: a channel failure is logged, never fatal to the backup.

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
        if let Err(e) = send_webhook(&subject, &outcome).await {
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
        NotifyChannel::Webhook => send_webhook(&subject, &outcome).await,
    }
}

fn render_body(o: &JobOutcome<'_>) -> String {
    let mut lines = vec![format!("Job: {}", o.job_name), format!("Type: {}", o.kind)];
    if !o.databases.is_empty() {
        lines.push(format!("Databases: {}", o.databases.join(", ")));
    }
    lines.push(format!("Status: {}", o.status));
    lines.push(format!("Message: {}", o.message));
    if let Some(s) = o.stats {
        lines.push(format!(
            "Backed up {} bytes: {} chunks, {} uploaded, {} reused.",
            s.bytes, s.chunks, s.uploaded, s.reused
        ));
    }
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

async fn send_webhook(subject: &str, o: &JobOutcome<'_>) -> anyhow::Result<()> {
    let url = secrets::get(WEBHOOK_SECRET_KEY)?
        .ok_or_else(|| anyhow::anyhow!("no webhook URL stored"))?;

    // `text` so a Slack incoming webhook renders it; structured fields for
    // generic consumers.
    let mut payload = serde_json::json!({
        "text": format!("{subject}\n{}", render_body(o)),
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
