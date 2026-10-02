use zbus::proxy;

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait LogindManager {
    /// Suspend the system; with `interactive` false, polkit never asks for a password.
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;

    /// Suspend, then hibernate after `HibernateDelaySec` or on low battery.
    fn suspend_then_hibernate(&self, interactive: bool) -> zbus::Result<()>;

    /// Hibernate the system.
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
}
