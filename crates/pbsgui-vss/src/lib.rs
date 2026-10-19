//! Volume Shadow Copy (VSS) requester support shared by the pbsgui engines.
//!
//! [`VolumeSnapshot`] snapshots a set of volumes so a file backup reads a
//! frozen, consistent image: files that are open or locked on the live volume
//! (a user profile's `NTUSER.DAT`, an open mailbox file) read normally from the
//! shadow copy. The AD engine builds its System State capture on the same
//! bindings and helpers.
//!
//! Windows only; on other platforms the crate is empty.

#[cfg(windows)]
pub mod bindings;
#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::*;

/// Join a shadow `device` and the part of live `path` after its volume root
/// `vol` (both compared case-insensitively). `None` if `path` is not under
/// `vol`.
#[cfg_attr(not(windows), allow(dead_code))]
fn shadow_path(device: &str, vol: &str, path: &str) -> Option<String> {
    // `C:` without the trailing separator is the volume root itself.
    if path.eq_ignore_ascii_case(vol.trim_end_matches('\\')) {
        return Some(format!("{device}\\"));
    }
    let head = path.get(..vol.len())?;
    if !head.eq_ignore_ascii_case(vol) {
        return None;
    }
    Some(format!("{device}\\{}", &path[vol.len()..]))
}

#[cfg(test)]
mod tests {
    use super::shadow_path;

    #[test]
    fn shadow_paths_map_under_the_device() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3";
        assert_eq!(
            shadow_path(dev, r"c:\", r"C:\Users\pat").as_deref(),
            Some(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3\Users\pat")
        );
        assert_eq!(
            shadow_path(dev, r"c:\", r"C:\").as_deref(),
            Some(&format!("{dev}\\")[..])
        );
        assert_eq!(
            shadow_path(dev, r"c:\", "C:").as_deref(),
            Some(&format!("{dev}\\")[..])
        );
        assert_eq!(shadow_path(dev, r"c:\", r"D:\data"), None);
    }
}
