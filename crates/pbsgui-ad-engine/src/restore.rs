//! Restore from an Active Directory backup.
//!
//! Two independent paths:
//!   - Whole-DC recovery: a System State restore performed in Directory Services
//!     Restore Mode (DSRM) for the disaster tail (forest recovery, last-DC loss).
//!     This produces artifacts plus a runbook; it is never framed as reverting the
//!     DC (that would trigger USN rollback). See M5.
//!   - Object-level partial restore: recreate deleted objects, roll back changed
//!     attributes, and restore subtrees back to LIVE AD over LDAP. This is a
//!     normal online operation that replicates out; no DSRM, no reboot, and no
//!     USN-rollback concern. See M7.

/// Restore from a backup (dev entry point).
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("Active Directory restore is not implemented yet")
}
