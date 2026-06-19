//! Run a job's pre/post script via the system shell, capturing its output.

/// Run `script` through the shell with the given environment variables.
/// Returns (exit code, combined stdout+stderr).
pub async fn run(script: &str, env: &[(String, String)]) -> anyhow::Result<(i32, String)> {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let output = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(script)
        .envs(env.iter().cloned())
        .output()
        .await?;

    let code = output.status.code().unwrap_or(-1);
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((code, combined))
}
