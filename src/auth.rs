use std::cell::Cell;
use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use nonstick::{AuthnFlags, ConversationAdapter, ErrorCode, Transaction, TransactionBuilder};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::{self, Instant};
use tracing::{error, info, warn};

use crate::cloexec_above_stderr;
use crate::password::{self, CAPACITY};

/// The PAM service; NixOS needs `security.pam.services.rust-wl-idle-manager = { };`.
const SERVICE: &str = "rust-wl-idle-manager";

/// How long the helper may take before it is killed and the attempt counts as failed.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Attempt {
    child: Child,
    deadline: Instant,
}

/// Start the helper (`--auth`) and write `password` to its stdin, which is then closed.
pub async fn start(password: &[u8]) -> io::Result<Attempt> {
    // This binary, even if its file has been replaced since.
    let mut child = cloexec_above_stderr(&mut Command::new("/proc/self/exe"))
        .arg0("rust-wl-idle-manager")
        .arg("--auth")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    // At most 1 KiB, which the empty pipe takes at once. Dropping stdin closes it.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(password).await?;
    Ok(Attempt {
        child,
        deadline: Instant::now() + TIMEOUT,
    })
}

/// Wait for the attempt in `slot` to end, empty the slot, and return whether the
/// password was right: the helper exited with 0. With no attempt, wait forever.
pub async fn finished(slot: &mut Option<Attempt>) -> bool {
    let Some(attempt) = slot else {
        return std::future::pending().await;
    };
    let ok = match time::timeout_at(attempt.deadline, attempt.child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            error!("waiting for the authentication helper: {e}");
            false
        }
        Err(_) => {
            warn!(
                "the authentication helper took over {} s; killing it",
                TIMEOUT.as_secs()
            );
            false
        }
    };
    *slot = None;
    ok
}

/// The `--auth` helper: check the password on stdin with PAM; exit with 0 if it is right.
pub fn helper() -> ExitCode {
    let mut buffer = [0; CAPACITY];
    let ok = match read_all(&mut buffer) {
        Ok(0) => false,
        Ok(len) => check(&buffer[..len]),
        Err(e) => {
            error!("reading the password: {e}");
            false
        }
    };
    password::wipe(&mut buffer);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn read_all(buffer: &mut [u8]) -> io::Result<usize> {
    let mut stdin = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let mut len = 0;
    while len < buffer.len() {
        match stdin.read(&mut buffer[len..])? {
            0 => break,
            n => len += n,
        }
    }
    Ok(len)
}

/// Whether `password` is the current user's, by PAM. Like GDM's reauthentication, a
/// failed account check (e.g. an expired password) is logged but does not fail: a lock
/// screen must never lock its user out.
fn check(password: &[u8]) -> bool {
    let Some(user) = user_name() else {
        error!("the current user has no name in the password database");
        return false;
    };
    let pam = TransactionBuilder::new_with_service(SERVICE)
        .username(&user)
        .build(Conversation::new(password).into_conversation());
    let mut pam = match pam {
        Ok(pam) => pam,
        Err(e) => {
            error!("starting PAM service {SERVICE}: {e}");
            return false;
        }
    };
    if let Err(e) = pam.authenticate(AuthnFlags::empty()) {
        info!("authentication failed: {e}");
        return false;
    }
    if let Err(e) = pam.account_management(AuthnFlags::empty()) {
        warn!("ignoring a failed PAM account check: {e}");
    }
    true
}

fn user_name() -> Option<OsString> {
    // SAFETY: getpwuid returns null or a pointer to an entry that stays valid until the
    // next getpwuid call; it is copied out at once, and this thread is the only one.
    unsafe {
        let entry = libc::getpwuid(libc::getuid()).as_ref()?;
        Some(OsStr::from_bytes(CStr::from_ptr(entry.pw_name).to_bytes()).to_owned())
    }
}

/// Answers PAM's first password prompt with the password. A second one aborts, as in
/// swaylock: pam_systemd_home asks again itself after a wrong password.
struct Conversation<'a> {
    password: &'a [u8],
    answered: Cell<bool>,
}

impl<'a> Conversation<'a> {
    fn new(password: &'a [u8]) -> Self {
        Self {
            password,
            answered: Cell::new(false),
        }
    }
}

impl ConversationAdapter for Conversation<'_> {
    fn prompt(&self, _: impl AsRef<OsStr>) -> nonstick::Result<OsString> {
        Err(ErrorCode::ConversationError)
    }

    /// nonstick takes an owned copy, which it cannot wipe; it dies with this process.
    fn masked_prompt(&self, _: impl AsRef<OsStr>) -> nonstick::Result<OsString> {
        if self.answered.replace(true) {
            return Err(ErrorCode::Abort);
        }
        Ok(OsStr::from_bytes(self.password).to_owned())
    }

    fn error_msg(&self, message: impl AsRef<OsStr>) {
        warn!("PAM: {}", message.as_ref().display());
    }

    fn info_msg(&self, message: impl AsRef<OsStr>) {
        info!("PAM: {}", message.as_ref().display());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;

    use super::*;

    #[test]
    fn only_the_first_password_prompt_is_answered() {
        let conversation = Conversation::new(b"pw");
        assert_eq!(conversation.masked_prompt("Password: ").unwrap(), "pw");
        let again = conversation.masked_prompt("Password: ");
        assert!(matches!(again, Err(ErrorCode::Abort)));
        assert!(conversation.prompt("Login: ").is_err());
    }

    fn attempt(program: &str, args: &[&str], deadline: Duration) -> Option<Attempt> {
        let child = Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + deadline;
        Some(Attempt { child, deadline })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_exit_status_is_the_answer() {
        let mut slot = attempt("true", &[], TIMEOUT);
        assert!(finished(&mut slot).await);
        assert!(slot.is_none());
        let mut slot = attempt("false", &[], TIMEOUT);
        assert!(!finished(&mut slot).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_helper_that_takes_too_long_is_killed_and_fails() {
        let mut slot = attempt("sleep", &["60"], Duration::from_millis(50));
        let pid = slot.as_ref().unwrap().child.id().unwrap();
        assert!(!finished(&mut slot).await);
        assert!(slot.is_none());
        // Gone, or a zombie until tokio reaps it.
        let dead = || {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stat.is_empty() || stat.contains(") Z ")
        };
        for _ in 0..100 {
            if dead() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the helper {pid} is still running");
    }
}
