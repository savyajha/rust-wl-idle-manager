//! The power row: what logind lets the greeter do, and doing it. `serve` runs as a task of
//! its own, so a slow or dead bus holds up nothing: until logind has answered, there is no
//! row, and with no answer there is none at all.

use anyhow::Context;
use futures_lite::StreamExt;
use tokio::sync::mpsc;
use tracing::{info, warn};
use ui::logind::LogindManagerProxy;

/// What a power button does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Sleep,
    Hibernate,
    Restart,
    ShutDown,
}

/// News from the power task.
#[derive(Clone, Debug, PartialEq)]
pub enum News {
    /// logind's answer: the row.
    Buttons(Vec<Action>),
    /// The action failed; logged there.
    Failed(Action),
    /// The machine woke from sleep or hibernation.
    Resumed,
}

const ACTIONS: [Action; 4] = [
    Action::Sleep,
    Action::Hibernate,
    Action::Restart,
    Action::ShutDown,
];

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Sleep => "Sleep",
            Action::Hibernate => "Hibernate",
            Action::Restart => "Restart",
            Action::ShutDown => "Shut Down",
        }
    }

    /// The dialog's question, for those that ask first: Restart and Shut Down, which end
    /// the login screen. Sleep and Hibernate lose nothing, so act at once.
    pub fn question(self) -> Option<&'static str> {
        match self {
            Action::Restart => Some("Restart now?"),
            Action::ShutDown => Some("Shut down now?"),
            Action::Sleep | Action::Hibernate => None,
        }
    }

    /// As the log has it: "shut down".
    pub fn name(self) -> String {
        self.label().to_lowercase()
    }
}

/// The actions shown for logind's `answers` to CanSuspend, CanHibernate, CanReboot and
/// CanPowerOff, in that order: those it says "yes" to. Any other answer ("no", "na",
/// "challenge", and systemd 261's "inhibited" and so on) hides it.
pub fn shown(answers: &[String; 4]) -> Vec<Action> {
    let yes = ACTIONS
        .into_iter()
        .zip(answers)
        .filter(|(_, answer)| *answer == "yes");
    yes.map(|(action, _)| action).collect()
}

/// Ask logind for the row, once, and send it; then do each action asked for (`interactive`
/// false: polkit never asks), and say when the machine wakes. With no system bus or logind,
/// no row.
pub async fn serve(news: mpsc::Sender<News>, mut actions: mpsc::Receiver<Action>) {
    if let Err(e) = follow(&news, &mut actions).await {
        info!("no power row: {e:#}");
    }
}

async fn follow(
    news: &mpsc::Sender<News>,
    actions: &mut mpsc::Receiver<Action>,
) -> anyhow::Result<()> {
    let bus = zbus::Connection::system()
        .await
        .context("connecting to the system bus")?;
    let logind = LogindManagerProxy::new(&bus).await?;
    let mut sleeps = logind.receive_prepare_for_sleep().await?;
    let answers = [
        logind.can_suspend().await?,
        logind.can_hibernate().await?,
        logind.can_reboot().await?,
        logind.can_power_off().await?,
    ];
    let [sleep, hibernate, restart, shut_down] = &answers;
    info!("power: sleep {sleep}, hibernate {hibernate}, restart {restart}, shut down {shut_down}");
    if news.send(News::Buttons(shown(&answers))).await.is_err() {
        return Ok(());
    }
    loop {
        let sent = tokio::select! {
            action = actions.recv() => {
                let Some(action) = action else { return Ok(()) };
                info!("power: {}", action.name());
                let done = match action {
                    Action::Sleep => logind.suspend(false).await,
                    Action::Hibernate => logind.hibernate(false).await,
                    Action::Restart => logind.reboot(false).await,
                    Action::ShutDown => logind.power_off(false).await,
                };
                match done {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        warn!("power: {} failed: {e}", action.name());
                        news.send(News::Failed(action)).await
                    }
                }
            }
            sleep = sleeps.next() => {
                let start = sleep.context("logind's signals ended")?.args()?.start;
                if start { Ok(()) } else { news.send(News::Resumed).await }
            }
        };
        if sent.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_yes_shows_a_button() {
        let answers = ["yes", "na", "challenge", "yes"].map(String::from);
        assert_eq!(shown(&answers), [Action::Sleep, Action::ShutDown]);
        let none = ["no", "inhibited", "challenge-inhibitor-blocked", ""].map(String::from);
        assert!(shown(&none).is_empty());
        assert_eq!(shown(&["yes"; 4].map(String::from)).len(), 4);
    }

    #[test]
    fn restart_and_shut_down_ask_first() {
        assert_eq!(Action::Restart.question(), Some("Restart now?"));
        assert_eq!(Action::ShutDown.question(), Some("Shut down now?"));
        assert_eq!(Action::Sleep.question(), None);
        assert_eq!(Action::Hibernate.question(), None);
        assert_eq!(Action::ShutDown.name(), "shut down");
    }
}
