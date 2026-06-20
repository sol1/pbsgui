//! Readiness checks for backing up a SQL Server instance.
//!
//! Each check returns a status and, when it is not Ok, a hint on how to fix it,
//! so the common snags (TCP/IP disabled, the backup service account not being
//! sysadmin) are caught before a backup is attempted rather than failing midway.

use pbsgui_ipc::{CheckStatus, SqlAuth, SqlCheck};

use super::probe::{self, SqlClient};

/// Run the readiness checks. Always returns a list; failures are reported as
/// check results rather than as an error.
pub async fn check(
    server: &str,
    port: Option<u16>,
    auth: &SqlAuth,
    password: Option<&str>,
) -> Vec<SqlCheck> {
    let mut checks = Vec::new();

    let mut client = match probe::connect(server, port, auth, password).await {
        Ok(client) => {
            checks.push(ok("Connectivity", "Connected to the instance."));
            client
        }
        Err(e) => {
            checks.push(fail(
                "Connectivity",
                &format!("{e:#}"),
                "Make sure the SQL Server service is running and TCP/IP is enabled in SQL \
                 Server Configuration Manager (Protocols > TCP/IP), then restart the service. \
                 If the error mentions 'login failed', the backup service account needs a SQL \
                 Server login on this instance.",
            ));
            return checks;
        }
    };

    let principal = scalar_string(&mut client, "SELECT SUSER_SNAME()").await;
    if let Some(name) = &principal {
        checks.push(ok("Login", &format!("Connected as {name}.")));
    }

    let is_sysadmin =
        scalar_i32(&mut client, "SELECT IS_SRVROLEMEMBER('sysadmin')").await == Some(1);
    if is_sysadmin {
        checks.push(ok(
            "sysadmin role",
            "The connection is a sysadmin, which VDI backup requires.",
        ));
    } else {
        let who = principal.as_deref().unwrap_or("your login");
        checks.push(fail(
            "sysadmin role",
            "VDI backup requires the connecting login to be in the sysadmin server role.",
            &format!(
                "In SSMS, connected as an administrator, run: \
                 ALTER SERVER ROLE sysadmin ADD MEMBER [{who}];"
            ),
        ));
    }

    checks
}

fn ok(name: &str, detail: &str) -> SqlCheck {
    SqlCheck {
        name: name.to_string(),
        status: CheckStatus::Ok,
        detail: detail.to_string(),
        hint: None,
    }
}

fn fail(name: &str, detail: &str, hint: &str) -> SqlCheck {
    SqlCheck {
        name: name.to_string(),
        status: CheckStatus::Fail,
        detail: detail.to_string(),
        hint: Some(hint.to_string()),
    }
}

async fn scalar_string(client: &mut SqlClient, sql: &str) -> Option<String> {
    let row = client
        .simple_query(sql)
        .await
        .ok()?
        .into_row()
        .await
        .ok()??;
    row.get::<&str, _>(0).map(str::to_string)
}

async fn scalar_i32(client: &mut SqlClient, sql: &str) -> Option<i32> {
    let row = client
        .simple_query(sql)
        .await
        .ok()?
        .into_row()
        .await
        .ok()??;
    row.get::<i32, _>(0)
}
