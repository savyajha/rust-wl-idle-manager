use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use anyhow::Context;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, Value};

/// Job mode that fails, rather than replacing a conflicting queued job.
const MODE_FAIL: &str = "fail";

/// D-Bus error systemd returns when a unit by the requested name is already loaded.
const UNIT_EXISTS: &str = "org.freedesktop.systemd1.UnitExists";

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait SystemdManager {
    /// Create the unit `name` from `properties` and start it; `aux` holds further units
    /// to create alongside it.
    fn start_transient_unit(
        &self,
        name: &str,
        mode: &str,
        properties: &[(&str, Value<'_>)],
        aux: &[(&str, &[(&str, Value<'_>)])],
    ) -> zbus::Result<OwnedObjectPath>;
}

impl SystemdManagerProxy<'_> {
    /// Run `argv` (not empty) as the transient service `unit`. Returns false, starting
    /// nothing, if a unit by that name is already loaded.
    pub async fn start_service(
        &self,
        unit: &str,
        description: &str,
        argv: &[String],
    ) -> anyhow::Result<bool> {
        let path = env::var("PATH").unwrap_or_default();
        let program = find_program(&argv[0], &path)
            .with_context(|| format!("{} not found in PATH", argv[0]))?;
        let properties = [
            ("Description", Value::from(description)),
            (
                "ExecStart",
                Value::from(vec![(program, argv.to_vec(), false)]),
            ),
            // Unload the unit once it stops, even if it failed, so its name is free again.
            ("CollectMode", Value::from("inactive-or-failed")),
        ];
        match self
            .start_transient_unit(unit, MODE_FAIL, &properties, &[])
            .await
        {
            Ok(_) => Ok(true),
            Err(zbus::Error::MethodError(name, _, _)) if name == UNIT_EXISTS => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The path to run for `program`, much like systemd-run finds it: as is if it contains
/// a '/', otherwise the first executable file named `program` in the directories of `path`.
fn find_program(program: &str, path: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_owned());
    }
    path.split(':')
        // systemd needs an absolute path, so skip empty and relative entries.
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| format!("{dir}/{program}"))
        .find(|file| {
            fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_program_takes_the_first_executable_file() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/target/find-program-test");
        let _ = fs::remove_dir_all(root);
        let (a, b) = (format!("{root}/a"), format!("{root}/b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(format!("{b}/dir")).unwrap();
        for (dir, name, mode) in [
            (&a, "both", 0o755),
            (&a, "plain", 0o644),
            (&b, "both", 0o755),
        ] {
            let file = format!("{dir}/{name}");
            fs::write(&file, "").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = format!("relative::{a}:{b}");

        assert_eq!(find_program("both", &path), Some(format!("{a}/both")));
        assert_eq!(find_program("plain", &path), None);
        assert_eq!(find_program("dir", &path), None);
        assert_eq!(find_program("missing", &path), None);
        assert_eq!(find_program("./x", &path), Some("./x".to_owned()));
        fs::remove_dir_all(root).unwrap();
    }
}
