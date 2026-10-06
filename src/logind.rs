use std::env;
use std::os::fd::OwnedFd;

use anyhow::Context;
use zbus::proxy;
use zbus::zvariant::{self, OwnedObjectPath};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait LogindManager {
    /// Take an inhibitor lock; it is held until the returned fd is closed.
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zvariant::OwnedFd>;

    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

    /// Suspend the system; with `interactive` false, polkit never asks for a password.
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;

    /// Suspend, then hibernate after `HibernateDelaySec` or on low battery.
    fn suspend_then_hibernate(&self, interactive: bool) -> zbus::Result<()>;

    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;

    /// What block-mode inhibitors are held for, colon-separated, e.g. "idle:sleep".
    #[zbus(property)]
    fn block_inhibited(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn lid_closed(&self) -> zbus::Result<bool>;

    /// Whether the system is docked or has more than one display; logind announces no
    /// change of it, so each read asks logind.
    #[zbus(property(emits_changed_signal = "false"))]
    fn docked(&self) -> zbus::Result<bool>;

    /// Emitted with `true` before sleep, and with `false` after waking or a failed sleep.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.login1.User",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/user/self"
)]
pub trait LogindUser {
    /// The user's primary session as (ID, path); the ID is empty if there is none.
    #[zbus(property)]
    fn display(&self) -> zbus::Result<(String, OwnedObjectPath)>;
}

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
pub trait LogindSession {
    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn unlock(&self) -> zbus::Result<()>;

    /// Set while the session is locked, by the compositor or the locker.
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;

    /// False while another session is in the foreground, e.g. on another VT.
    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
}

impl LogindManagerProxy<'_> {
    /// Our session's ID and a proxy for it: the user's primary session, or else the
    /// one named by `XDG_SESSION_ID`.
    pub async fn our_session(&self) -> anyhow::Result<(String, LogindSessionProxy<'static>)> {
        let conn = self.inner().connection();
        let (mut id, mut path) = LogindUserProxy::new(conn)
            .await?
            .display()
            .await
            .context("reading the user's primary session from logind")?;
        if id.is_empty() {
            id = env::var("XDG_SESSION_ID")
                .context("no logind session: the user has none and XDG_SESSION_ID is not set")?;
            path = self
                .get_session(&id)
                .await
                .with_context(|| format!("looking up session {id} from XDG_SESSION_ID"))?;
        }
        Ok((id, LogindSessionProxy::new(conn, path).await?))
    }

    pub async fn sleep_inhibitor(&self) -> anyhow::Result<OwnedFd> {
        let fd = self
            .inhibit(
                "sleep",
                "rust-wl-idle-manager",
                "lock the session before sleep",
                "delay",
            )
            .await
            .context("taking the sleep inhibitor")?;
        Ok(fd.into())
    }
}

/// Whether logind's `BlockInhibited` list holds `idle`.
pub fn idle_inhibited(block_inhibited: &str) -> bool {
    block_inhibited.split(':').any(|what| what == "idle")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_inhibited_needs_the_whole_element() {
        assert!(idle_inhibited("idle"));
        assert!(idle_inhibited("sleep:idle:handle-lid-switch"));
        assert!(!idle_inhibited(""));
        assert!(!idle_inhibited("sleep:shutdown"));
        assert!(!idle_inhibited("idlex:xidle"));
    }
}
