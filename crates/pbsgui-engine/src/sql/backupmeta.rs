//! Per-snapshot SQL backup metadata, used to rebuild a restore chain for
//! point-in-time recovery.
//!
//! Each SQL snapshot carries a small [`META_BLOB_NAME`] blob with the backup's
//! type and LSN range (captured from `msdb.dbo.backupset` right after `BACKUP`).
//! [`select_chain`] uses those to pick the full plus the ordered log backups that
//! carry a database forward to a target time.

use serde::{Deserialize, Serialize};

/// Name of the metadata blob stored inside each SQL snapshot.
pub const META_BLOB_NAME: &str = "meta.json.blob";

/// Metadata for one SQL backup (full or log).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlBackupMeta {
    /// "full" or "log".
    pub kind: String,
    /// Backup finish time, unix seconds.
    pub backup_time: i64,
    /// First/last log sequence numbers (decimal strings, as `msdb` stores them).
    pub first_lsn: String,
    pub last_lsn: String,
    /// LSN of the full this backup chains from (for diagnostics).
    #[serde(default)]
    pub database_backup_lsn: String,
}

impl SqlBackupMeta {
    pub fn is_log(&self) -> bool {
        self.kind.eq_ignore_ascii_case("log")
    }
}

/// One candidate snapshot: its PBS address time plus its metadata.
#[derive(Debug, Clone)]
pub struct ChainItem {
    /// The snapshot's PBS backup-time (how it is addressed for download).
    pub snapshot_time: i64,
    pub meta: SqlBackupMeta,
}

/// Parse a decimal LSN string to a comparable integer (0 if unparseable). SQL
/// LSNs are `numeric(25,0)`, which fits in `u128`.
fn parse_lsn(s: &str) -> u128 {
    s.trim().parse().unwrap_or(0)
}

/// Select the restore chain to bring a database to `target` (unix seconds): the
/// latest full at or before `target`, then the log backups (ordered by LSN) that
/// carry it forward to `target`. Returned full-first, in apply order. Empty if no
/// full covers the target.
pub fn select_chain(items: &[ChainItem], target: i64) -> Vec<ChainItem> {
    let full = items
        .iter()
        .filter(|i| !i.meta.is_log() && i.meta.backup_time <= target)
        .max_by_key(|i| i.meta.backup_time);
    let Some(full) = full else {
        return Vec::new();
    };

    // After restoring the full, the database sits at the full's last_lsn. The
    // logs that carry it forward are those whose range *ends* past that point
    // (last_lsn > base) - including the "bridging" log that began before the full
    // finished. Filtering on first_lsn would skip that bridging log and leave a
    // gap (SQL error 4305, "the log ... is too recent to apply").
    let base_lsn = parse_lsn(&full.meta.last_lsn);
    let mut logs: Vec<&ChainItem> = items
        .iter()
        .filter(|i| i.meta.is_log() && parse_lsn(&i.meta.last_lsn) > base_lsn)
        .collect();
    logs.sort_by_key(|i| parse_lsn(&i.meta.first_lsn));

    let mut chain = vec![full.clone()];
    for log in logs {
        chain.push(log.clone());
        // Stop once a log carries the database to or past the target; STOPAT then
        // trims it to the exact moment.
        if log.meta.backup_time >= target {
            break;
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(t: i64, first: &str, last: &str) -> ChainItem {
        ChainItem {
            snapshot_time: t,
            meta: SqlBackupMeta {
                kind: "full".into(),
                backup_time: t,
                first_lsn: first.into(),
                last_lsn: last.into(),
                database_backup_lsn: String::new(),
            },
        }
    }
    fn log(t: i64, first: &str, last: &str) -> ChainItem {
        ChainItem {
            snapshot_time: t,
            meta: SqlBackupMeta {
                kind: "log".into(),
                backup_time: t,
                first_lsn: first.into(),
                last_lsn: last.into(),
                database_backup_lsn: String::new(),
            },
        }
    }

    #[test]
    fn picks_latest_full_then_logs_to_target() {
        let items = vec![
            full(100, "10", "20"),
            full(200, "30", "40"), // newer full -> chain base for target >= 200
            log(210, "40", "50"),
            log(220, "50", "60"),
            log(230, "60", "70"),
        ];
        // Target between log@220 and log@230: full@200 + logs up to the one that
        // covers it (the log@230, first with backup_time >= target).
        let chain = select_chain(&items, 225);
        let times: Vec<i64> = chain.iter().map(|c| c.snapshot_time).collect();
        assert_eq!(times, vec![200, 210, 220, 230]);
        assert!(!chain[0].meta.is_log());
    }

    #[test]
    fn target_before_any_log_is_full_plus_first_log() {
        let items = vec![
            full(200, "30", "40"),
            log(210, "40", "50"),
            log(220, "50", "60"),
        ];
        // Target right after the full, before the first log finishes: full + the
        // first log (which contains the target moment), trimmed by STOPAT.
        let chain = select_chain(&items, 205);
        assert_eq!(
            chain.iter().map(|c| c.snapshot_time).collect::<Vec<_>>(),
            vec![200, 210]
        );
    }

    #[test]
    fn target_at_latest_includes_all_logs() {
        let items = vec![
            full(200, "30", "40"),
            log(210, "40", "50"),
            log(220, "50", "60"),
        ];
        let chain = select_chain(&items, 220);
        assert_eq!(
            chain.iter().map(|c| c.snapshot_time).collect::<Vec<_>>(),
            vec![200, 210, 220]
        );
    }

    #[test]
    fn no_full_covering_target_is_empty() {
        let items = vec![full(200, "30", "40"), log(210, "40", "50")];
        assert!(select_chain(&items, 150).is_empty());
    }

    #[test]
    fn includes_the_log_that_bridges_the_full() {
        // The first log after a full begins *before* the full's last_lsn (it was
        // running while the full was taken) and ends after it. It must be applied
        // first, or SQL rejects the next log as "too recent" (error 4305).
        let items = vec![
            full(200, "30", "40"),
            log(210, "35", "50"), // bridging: first_lsn 35 < full last_lsn 40
            log(220, "50", "60"),
        ];
        let chain = select_chain(&items, 215);
        assert_eq!(
            chain.iter().map(|c| c.snapshot_time).collect::<Vec<_>>(),
            vec![200, 210, 220]
        );
    }

    #[test]
    fn ignores_logs_from_an_earlier_chain() {
        // A log whose first_lsn precedes the chosen full's last_lsn belongs to an
        // earlier chain and must not be included.
        let items = vec![
            full(200, "30", "40"),
            log(150, "20", "30"), // older chain
            log(210, "40", "50"),
        ];
        let chain = select_chain(&items, 215);
        assert_eq!(
            chain.iter().map(|c| c.snapshot_time).collect::<Vec<_>>(),
            vec![200, 210]
        );
    }
}
