//! SQL Server topology detection and VDI streaming backup.
//!
//! Detection runs first and decides the backup strategy. The detection queries
//! below are run through a TDS client (tiberius). The actual backup byte stream
//! is driven over the Virtual Device Interface (VDI): a `BACKUP DATABASE/LOG ...
//! TO VIRTUAL_DEVICE = '<name>'` statement is issued through tiberius while a
//! native COM loop on `SQLVDI.dll` reads SQL's backup buffers and forwards them
//! to PBS. The VDI connection must be `sysadmin`.

pub mod backupmeta;
pub mod check;
pub mod discover;
pub mod probe;
pub mod ssrp;
pub mod vdi;
