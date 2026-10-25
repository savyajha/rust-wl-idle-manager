//! The users who can log in, as GDM and LightDM list them without AccountsService: those of
//! `/etc/passwd` with a UID in login.defs's range for ordinary users and a login shell.
//! The nix crate has no way to list the passwd database, so the file is read directly.

use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

use tracing::{info, warn};

const PASSWD: &str = "/etc/passwd";
const LOGIN_DEFS: &str = "/etc/login.defs";

/// The UIDs of ordinary users where login.defs has no `UID_MIN` or `UID_MAX`.
const DEFAULT_UIDS: RangeInclusive<u32> = 1000..=60000;

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    /// The user name greetd logs in.
    pub name: String,
    /// The GECOS field's name, or else the user name.
    pub full_name: String,
}

/// The users who can log in, sorted by full name; none if `/etc/passwd` cannot be read.
pub fn load() -> Vec<User> {
    let defs = read(Path::new(LOGIN_DEFS)).unwrap_or_else(|e| {
        warn!("reading {LOGIN_DEFS}: {e}; taking UIDs {DEFAULT_UIDS:?}");
        String::new()
    });
    let passwd = read(Path::new(PASSWD)).unwrap_or_else(|e| {
        warn!("reading {PASSWD}: {e}; listing no users");
        String::new()
    });
    let users = list(&passwd, uid_range(&defs));
    let names: Vec<_> = users.iter().map(|user| user.full_name.as_str()).collect();
    info!("users: {}", names.join(", "));
    users
}

/// The text at `path`, a byte that is not UTF-8 (in someone's GECOS name, say) replaced
/// rather than losing the whole file.
fn read(path: &Path) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// login.defs's `UID_MIN..=UID_MAX` in `defs`, each where missing or invalid from
/// `DEFAULT_UIDS`. Like shadow, the last of a key's lines counts.
fn uid_range(defs: &str) -> RangeInclusive<u32> {
    let value = |key: &str| {
        defs.lines().rev().find_map(
            |line| match line.split_whitespace().collect::<Vec<_>>()[..] {
                [name, value, ..] if name == key => value.parse().ok(),
                _ => None,
            },
        )
    };
    let min = value("UID_MIN").unwrap_or(*DEFAULT_UIDS.start());
    let max = value("UID_MAX").unwrap_or(*DEFAULT_UIDS.end());
    min..=max
}

/// The users of `passwd` (`/etc/passwd`'s text) with a UID in `uids` and a shell that is
/// not `nologin` or `false`, sorted by full name, ignoring case.
fn list(passwd: &str, uids: RangeInclusive<u32>) -> Vec<User> {
    let mut users = Vec::new();
    for line in passwd.lines() {
        let [name, _, uid, _, gecos, _, shell] = line.split(':').collect::<Vec<_>>()[..] else {
            continue;
        };
        let can_log_in = !shell.ends_with("nologin") && !shell.ends_with("false");
        if uid.parse().is_ok_and(|uid| uids.contains(&uid)) && can_log_in {
            users.push(User {
                name: name.to_owned(),
                full_name: full_name(name, gecos),
            });
        }
    }
    users.sort_by_cached_key(|user| (user.full_name.to_lowercase(), user.name.clone()));
    users
}

/// The name in `gecos`, up to its first comma, or without one, the user `name`.
fn full_name(name: &str, gecos: &str) -> String {
    let full = gecos.split(',').next().unwrap_or_default().trim();
    if full.is_empty() { name } else { full }.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uid_range_comes_from_login_defs_or_the_defaults() {
        let defs = "# UID_MIN 1\nUID_MIN\t\t 500\nMAIL_DIR /var/mail\nUID_MAX 29999 # end\n";
        assert_eq!(uid_range(defs), 500..=29999);
        // The last line counts; a missing or invalid value is the default.
        assert_eq!(uid_range("UID_MIN 500\nUID_MIN 2000\n"), 2000..=60000);
        assert_eq!(uid_range("UID_MIN lots\nUID_MAX\n"), DEFAULT_UIDS);
        assert_eq!(uid_range(""), DEFAULT_UIDS);
    }

    #[test]
    fn only_ordinary_users_with_a_login_shell_are_listed_by_full_name() {
        let passwd = "\
root:x:0:0:System administrator:/root:/run/current-system/sw/bin/bash
zed:x:1001:100:Zed,Room 1,,:/home/zed:/bin/zsh
alice:x:1000:100:Alice Liddell:/home/alice:/run/current-system/sw/bin/bash
bob:x:1002:100::/home/bob:
nobody:x:65534:65534:Unprivileged:/var/empty:/run/current-system/sw/bin/nologin
carol:x:1003:100:Carol:/home/carol:/run/current-system/sw/bin/nologin
dave:x:1004:100:Dave:/home/dave:/bin/false
nixbld1:x:30001:30000:Nix build user 1:/var/empty:/bin/bash
greeter:x:999:999::/var/empty:/bin/sh
+nis
broken:x:1005
";
        let users = list(passwd, 1000..=29999);
        let names: Vec<_> = users.iter().map(|u| (&*u.name, &*u.full_name)).collect();
        // An empty shell is /bin/sh; an empty GECOS name is the user name. Case is ignored.
        assert_eq!(
            names,
            [("alice", "Alice Liddell"), ("bob", "bob"), ("zed", "Zed")]
        );
        assert!(list("", DEFAULT_UIDS).is_empty());
    }

    #[test]
    fn a_byte_that_is_not_utf8_loses_no_one() {
        let path = std::env::temp_dir().join(format!("greeter-passwd-{}", std::process::id()));
        let passwd = b"alice:x:1000:100:Al\xffice:/h:/bin/sh\nbob:x:1001:100::/h:/bin/sh\n";
        fs::write(&path, passwd).unwrap();
        let text = read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let users = list(&text, DEFAULT_UIDS);
        let names: Vec<_> = users.iter().map(|u| (&*u.name, &*u.full_name)).collect();
        assert_eq!(names, [("alice", "Al\u{fffd}ice"), ("bob", "bob")]);
        assert!(read(&path).is_err());
    }

    #[test]
    fn the_full_name_is_gecos_up_to_its_first_comma() {
        assert_eq!(
            full_name("alice", "Alice Liddell,Room 1,,"),
            "Alice Liddell"
        );
        assert_eq!(full_name("alice", " ,Room 1"), "alice");
        assert_eq!(full_name("alice", ""), "alice");
    }
}
