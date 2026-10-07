use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, ensure};
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, Value};

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait SystemdManager {
    fn start_transient_unit(
        &self,
        name: &str,
        mode: &str,
        properties: &[(&str, Value<'_>)],
        aux: &[(&str, &[(&str, Value<'_>)])],
    ) -> zbus::Result<OwnedObjectPath>;

    fn kill_unit(&self, name: &str, whom: &str, signal: i32) -> zbus::Result<()>;
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
        let program = find_program(&argv[0], &path)?;
        let properties = [
            ("Description", Value::from(description)),
            (
                "ExecStart",
                Value::from(vec![(program, argv.to_vec(), false)]),
            ),
            // Unload the unit once it stops, even if it failed, so its name is free again.
            ("CollectMode", Value::from("inactive-or-failed")),
        ];
        // Job mode "fail": refuse a conflicting job rather than replace it.
        let started = self
            .start_transient_unit(unit, "fail", &properties, &[])
            .await;
        Ok(unless_error(started, "UnitExists")?)
    }

    /// Send `signal` to the main process of `unit`. Returns false, sending nothing, if
    /// the unit is not loaded.
    pub async fn signal_main(&self, unit: &str, signal: i32) -> zbus::Result<bool> {
        unless_error(self.kill_unit(unit, "main", signal).await, "NoSuchUnit")
    }
}

/// Whether `result` is a success; `Ok(false)` for systemd's D-Bus error `error`.
fn unless_error<T>(result: zbus::Result<T>, error: &str) -> zbus::Result<bool> {
    match result {
        Ok(_) => Ok(true),
        Err(zbus::Error::MethodError(name, ..))
            if name.strip_prefix("org.freedesktop.systemd1.") == Some(error) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// The path to run for `program`, much like systemd-run finds it: as is if absolute,
/// otherwise the first executable file named `program` in the directories of `path`.
/// A relative path with a '/' is an error, since systemd needs an absolute one.
fn find_program(program: &str, path: &str) -> anyhow::Result<String> {
    if program.starts_with('/') {
        return Ok(program.to_owned());
    }
    ensure!(
        !program.contains('/'),
        "{program}: a program path must be absolute or a bare name"
    );
    path.split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| format!("{dir}/{program}"))
        .find(|file| {
            fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .with_context(|| format!("{program} not found in PATH"))
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
        let find = |program| find_program(program, &path).map_err(|e| e.to_string());

        assert_eq!(find("both"), Ok(format!("{a}/both")));
        assert_eq!(find("plain"), Err("plain not found in PATH".to_owned()));
        assert_eq!(find("dir"), Err("dir not found in PATH".to_owned()));
        assert_eq!(find("missing"), Err("missing not found in PATH".to_owned()));
        assert_eq!(find("/x/y"), Ok("/x/y".to_owned()));
        let relative = "./x: a program path must be absolute or a bare name";
        assert_eq!(find("./x"), Err(relative.to_owned()));
        fs::remove_dir_all(root).unwrap();
    }
}
