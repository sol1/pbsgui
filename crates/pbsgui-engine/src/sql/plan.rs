//! What this machine does with one database on one run: back it up (and how),
//! or skip it.
//!
//! The rules matter most on an Always On availability group, where the same job
//! runs on every replica and SQL Server restricts what a secondary may do: a
//! secondary can take only copy-only full backups (before SQL Server 2025), and a
//! copy-only full does not start the log chain. So for point-in-time protection:
//!
//! - **Fulls run on the primary**, as normal (chain-starting) backups, whatever
//!   the group's backup preference. Secondaries skip them.
//! - **Log backups run on the preferred backup replica** (the group's
//!   `AUTOMATED_BACKUP_PREFERENCE`, default "prefer secondary"). A log backup on
//!   a secondary is a normal log backup: it truncates the log group-wide.
//! - **A log run with no chain to extend takes a full instead** on the primary
//!   or a standalone instance (a new database, a recovery model change, or no
//!   full yet). This runs even on a primary that is not the preferred replica,
//!   so a log backup on the secondary can follow. A secondary cannot do this; if
//!   its log backup finds no chain, the run warns that the primary must take one.
//!
//! Fulls-only plans keep the simpler rule: back up on the preferred replica, as a
//! copy-only full on a secondary.

use pbsgui_ipc::SqlBackupType;

use crate::backup::SqlRun;

/// The protection plan, reduced to what the rules need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    /// Fulls plus log backups.
    PointInTime,
    /// Fulls only.
    RestorePoints,
    /// Copy-only fulls only, alongside another backup tool.
    SecondaryCopy,
}

/// This instance's role for one database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Not in an availability group.
    Standalone,
    /// The availability group primary.
    Primary,
    /// A secondary (or a replica still resolving its role).
    Secondary,
}

/// What SQL Server reports about one database on this instance.
#[derive(Clone, Copy, Debug)]
pub struct DbState {
    pub role: Role,
    /// `sys.fn_hadr_backup_is_preferred_replica`; always true when standalone.
    pub preferred: bool,
    pub simple_recovery: bool,
    /// A log backup can extend the chain (`last_log_backup_lsn` is set). Only
    /// trusted on a primary or standalone instance, where it is the database's
    /// own state; a secondary's log backup is attempted and its error read.
    pub log_chain: bool,
}

/// The decision for one database.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Backup {
        kind: SqlBackupType,
        copy_only: bool,
        /// Why this differs from the run's usual backup, for the run log.
        note: Option<&'static str>,
    },
    /// Expected; nothing to do (the reason is for the run log).
    Skip(&'static str),
}

pub const NOT_PREFERRED: &str = "not the Always On preferred backup replica";
pub const FULL_ON_PRIMARY: &str =
    "point-in-time full backups are taken on the Always On primary replica";
pub const SIMPLE: &str = "SIMPLE recovery model (no log backups; log is auto-managed)";
pub const PROMOTED: &str =
    "no full backup starts the log chain yet, so taking a full backup instead of a log backup";
pub const NO_CHAIN_ON_SECONDARY: &str =
    "no full backup starts the log chain yet, and a secondary cannot take one; pbsgui on the \
     primary replica takes it on its next run (install pbsgui on every replica with this job)";

/// Decide what to do with one database.
pub fn decide(plan: PlanKind, run: SqlRun, s: &DbState) -> Action {
    let full = |copy_only, note| Action::Backup {
        kind: SqlBackupType::Full,
        copy_only,
        note,
    };
    match plan {
        PlanKind::PointInTime => match run {
            SqlRun::Full => match s.role {
                Role::Standalone | Role::Primary => full(false, None),
                Role::Secondary => Action::Skip(FULL_ON_PRIMARY),
            },
            SqlRun::Log => {
                if s.simple_recovery {
                    return Action::Skip(SIMPLE);
                }
                match s.role {
                    Role::Standalone | Role::Primary if !s.log_chain => full(false, Some(PROMOTED)),
                    _ if !s.preferred => Action::Skip(NOT_PREFERRED),
                    _ => Action::Backup {
                        kind: SqlBackupType::Log,
                        copy_only: false,
                        note: None,
                    },
                }
            }
        },
        PlanKind::RestorePoints | PlanKind::SecondaryCopy => {
            if !s.preferred {
                return Action::Skip(NOT_PREFERRED);
            }
            // A secondary may only take a copy-only full.
            let copy_only = plan == PlanKind::SecondaryCopy || s.role == Role::Secondary;
            full(copy_only, None)
        }
    }
}

/// Whether a failed log backup failed because no full backup starts the chain
/// (SQL Server error 4214), which on a secondary is a warning, not a failure.
pub fn is_no_chain_error(message: &str) -> bool {
    message.contains("4214") || message.contains("no current database backup")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(role: Role, preferred: bool, log_chain: bool) -> DbState {
        DbState {
            role,
            preferred,
            simple_recovery: false,
            log_chain,
        }
    }

    fn backup(kind: SqlBackupType, copy_only: bool) -> Action {
        Action::Backup {
            kind,
            copy_only,
            note: None,
        }
    }

    use SqlBackupType::{Full, Log};

    #[test]
    fn standalone_point_in_time() {
        let s = state(Role::Standalone, true, true);
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &s),
            backup(Full, false)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &s),
            backup(Log, false)
        );
        // No chain yet (new database): the log run takes a full instead.
        let s = state(Role::Standalone, true, false);
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &s),
            Action::Backup {
                kind: Full,
                copy_only: false,
                note: Some(PROMOTED)
            }
        );
    }

    #[test]
    fn ag_prefer_secondary_splits_fulls_and_logs() {
        // The default preference: the secondary is preferred.
        let primary = state(Role::Primary, false, true);
        let secondary = state(Role::Secondary, true, true);
        // Fulls: a normal full on the primary only, never copy-only.
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &primary),
            backup(Full, false)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &secondary),
            Action::Skip(FULL_ON_PRIMARY)
        );
        // Logs: on the preferred secondary only.
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &primary),
            Action::Skip(NOT_PREFERRED)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &secondary),
            backup(Log, false)
        );
    }

    #[test]
    fn ag_prefer_primary_does_everything_on_the_primary() {
        let primary = state(Role::Primary, true, true);
        let secondary = state(Role::Secondary, false, true);
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &primary),
            backup(Full, false)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &primary),
            backup(Log, false)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &secondary),
            Action::Skip(FULL_ON_PRIMARY)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &secondary),
            Action::Skip(NOT_PREFERRED)
        );
    }

    #[test]
    fn primary_repairs_a_missing_chain_even_when_not_preferred() {
        // The reported failure: logs on the secondary, no chain. The primary's
        // next log run takes the full that starts it.
        let primary = state(Role::Primary, false, false);
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &primary),
            Action::Backup {
                kind: Full,
                copy_only: false,
                note: Some(PROMOTED)
            }
        );
        // A secondary never promotes (it cannot start a chain): it attempts the
        // log backup, whose 4214 error becomes a warning.
        let secondary = state(Role::Secondary, true, false);
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &secondary),
            backup(Log, false)
        );
    }

    #[test]
    fn simple_recovery_never_takes_logs() {
        let mut s = state(Role::Primary, true, false);
        s.simple_recovery = true;
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Log, &s),
            Action::Skip(SIMPLE)
        );
        assert_eq!(
            decide(PlanKind::PointInTime, SqlRun::Full, &s),
            backup(Full, false)
        );
    }

    #[test]
    fn fulls_only_plans_keep_the_preferred_replica_rule() {
        let secondary = state(Role::Secondary, true, true);
        let primary = state(Role::Primary, false, true);
        assert_eq!(
            decide(PlanKind::RestorePoints, SqlRun::Full, &secondary),
            backup(Full, true)
        );
        assert_eq!(
            decide(PlanKind::RestorePoints, SqlRun::Full, &primary),
            Action::Skip(NOT_PREFERRED)
        );
        let standalone = state(Role::Standalone, true, true);
        assert_eq!(
            decide(PlanKind::RestorePoints, SqlRun::Full, &standalone),
            backup(Full, false)
        );
        assert_eq!(
            decide(PlanKind::SecondaryCopy, SqlRun::Full, &standalone),
            backup(Full, true)
        );
    }

    #[test]
    fn recognises_the_no_chain_error() {
        assert!(is_no_chain_error(
            "Token error: 'BACKUP LOG cannot be performed because there is no current database \
             backup.' on server SQL2 executing  on line 1 (code: 4214, state: 1, class: 16)"
        ));
        assert!(!is_no_chain_error("login failed (code: 18456)"));
    }
}
