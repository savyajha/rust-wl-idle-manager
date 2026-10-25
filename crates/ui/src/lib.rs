//! What the lock screen draws and types with: its layout, colours, wallpaper, drawing,
//! password entry and keyboard, and the Wayland queue and buffers it draws through.

pub mod config;
pub mod draw;
pub mod entry;
pub mod frames;
pub mod keyboard;
pub mod password;
pub mod queue;
pub mod theme;
pub mod wallpaper;

/// Mark every fd above stderr close-on-exec in `command`'s child: an fd received over D-Bus
/// (the sleep inhibitor's) lacks it until zbus drops its message (see DESIGN.md).
pub fn cloexec_above_stderr(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    // SAFETY: close_range is a system call, safe between fork and exec. It keeps the pipe std
    // reports a failed exec on; a failure (before Linux 5.11, or seccomp) is ignored.
    unsafe {
        command.pre_exec(|| {
            libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as i32);
            Ok(())
        })
    }
}
