//! Mount options and helper names shared by the Linux and macOS mount paths.

use std::error::Error as StdError;
use std::ffi::OsStr;
use std::path::Path;

use crate::fs::Owner;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub read_only: bool,
    pub noexec: bool,
    /// macOS `nobrowse`: keep the volume out of Finder.
    pub nobrowse: bool,
    pub strict: bool,
    /// `sync` or `dirsync`: publish every change before replying.
    pub sync: bool,
    pub owner: Owner,
}

/// Parse mount(8) and udisks options. Unknown options are rejected rather than ignored.
pub fn parse_options(text: &str, owner: Owner) -> Result<Options> {
    let mut metadata_cache = false;
    let mut options = Options {
        read_only: false,
        noexec: false,
        nobrowse: false,
        strict: false,
        sync: false,
        owner,
    };
    // Userspace `x-` options, such as x-gvfs-show, are for other programs.
    for option in text
        .split(',')
        .filter(|option| !option.is_empty() && !option.starts_with("x-"))
    {
        match option.split_once('=') {
            None => match option {
                "ro" => options.read_only = true,
                "rw" => options.read_only = false,
                // DiskArbitration's spelling of `ro`, as for write-protected
                // media and `diskutil mount readOnly`.
                #[cfg(target_os = "macos")]
                "rdonly" => options.read_only = true,
                #[cfg(target_os = "macos")]
                "nobrowse" => options.nobrowse = true,
                #[cfg(target_os = "macos")]
                "browse" => options.nobrowse = false,
                "noexec" => options.noexec = true,
                "exec" => options.noexec = false,
                "strict" => options.strict = true,
                // Read-only mounts always cache metadata; the option remains
                // accepted for existing fstab and udisks configurations.
                "metadata-cache" => metadata_cache = true,
                // Directory changes are not published apart from file data,
                // so `dirsync` asks for the same as `sync`.
                "sync" | "dirsync" => options.sync = true,
                "async" => options.sync = false,
                // Always applied, or meaningless: access times are not stored,
                // and ownership is neither stored nor checked (DiskArbitration
                // passes noowners on macOS).
                "nodev" | "nosuid" | "defaults" | "auto" | "noauto" | "nofail" | "atime"
                | "noatime" | "relatime" | "strictatime" | "nodiratime" | "lazytime" | "owners"
                | "noowners" => {}
                _ => return Err(format!("unsupported mount option: {option}").into()),
            },
            Some(("uid", value)) => options.owner.uid = value.parse()?,
            Some(("gid", value)) => options.owner.gid = value.parse()?,
            // Full plus listings are the only policy; the option stays
            // accepted for existing fstab and udisks configurations.
            #[cfg(target_os = "linux")]
            Some(("readdirplus", "yes")) => {}
            Some(("uhelper", _)) => {}
            Some(_) => return Err(format!("unsupported mount option: {option}").into()),
        }
    }
    if metadata_cache && !options.read_only {
        return Err("metadata-cache requires a read-only mount".into());
    }
    Ok(options)
}

/// Whether the program runs as mount(8)'s helper: `mount.carryfs` on Linux,
/// `mount_carryfs` on macOS.
pub fn is_helper_name(argv0: &OsStr) -> bool {
    matches!(
        Path::new(argv0).file_name().and_then(OsStr::to_str),
        Some("mount.carryfs" | "mount_carryfs")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Owner = Owner { uid: 0, gid: 0 };

    #[test]
    fn udisks_and_fstab_options_parse_and_unknown_options_fail() {
        let options = parse_options(
            "rw,uhelper=udisks2,nodev,nosuid,uid=1000,gid=1001,x-gvfs-show,noexec",
            ROOT,
        )
        .unwrap();
        assert_eq!(
            options,
            Options {
                read_only: false,
                noexec: true,
                nobrowse: false,
                strict: false,
                sync: false,
                owner: Owner {
                    uid: 1000,
                    gid: 1001
                },
            }
        );
        assert!(parse_options("ro", ROOT).unwrap().read_only);
        assert!(parse_options("ro,metadata-cache", ROOT).unwrap().read_only);
        assert!(parse_options("metadata-cache", ROOT).is_err());
        assert!(parse_options("ro,metadata-cache,rw", ROOT).is_err());
        assert!(parse_options("nodev,noowners,nosuid", ROOT).is_ok());
        assert!(parse_options("strict", ROOT).unwrap().strict);
        assert!(parse_options("rw,strict,uid=1000", ROOT).unwrap().strict);
        assert!(parse_options("rw,dirsync", ROOT).unwrap().sync);
        assert!(!parse_options("sync,async", ROOT).unwrap().sync);
        #[cfg(target_os = "macos")]
        {
            let options = parse_options("noowners,rdonly,nobrowse", ROOT).unwrap();
            assert!(options.read_only && options.nobrowse);
            assert!(!parse_options("nobrowse,browse", ROOT).unwrap().nobrowse);
        }
        #[cfg(target_os = "linux")]
        {
            assert!(parse_options("rdonly", ROOT).is_err());
            assert!(parse_options("nobrowse", ROOT).is_err());
            assert!(parse_options("readdirplus=yes", ROOT).is_ok());
            assert!(parse_options("readdirplus=auto", ROOT).is_err());
            assert!(parse_options("readdirplus=no", ROOT).is_err());
        }
        for bad in [
            "suid",
            "dev",
            "uid=root",
            "big_writes",
            "umask=022",
            "rw,io_uring",
        ] {
            assert!(parse_options(bad, ROOT).is_err(), "{bad}");
        }
    }

    #[test]
    fn helper_is_recognized_by_program_name() {
        assert!(is_helper_name(OsStr::new("/usr/sbin/mount.carryfs")));
        assert!(is_helper_name(OsStr::new(
            "/Library/Filesystems/carryfs.fs/Contents/Resources/mount_carryfs"
        )));
        assert!(!is_helper_name(OsStr::new("carryfs-fuse")));
    }
}
