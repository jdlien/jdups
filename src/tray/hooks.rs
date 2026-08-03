//! Power-event hooks: run a configured command when the power state changes.
//!
//! The design and its reasoning live in docs/power-hooks.md. The load-bearing
//! choices, restated where the code is: the **tray** executes the commands
//! because it is the user's session and the user's privileges, and the SYSTEM
//! agent must never execute a configured command line; the layer is
//! **state-driven**, so a command fires only when the lights should actually
//! change and every cancellation path falls out correctly; and failures are
//! silent, because a lighting command that fails is a cosmetic problem in the
//! cosmetic process.

use jdups::config::Settings;
use jdups::model::Power;

/// What the room should look like. One of three, and `Pending` outranks the
/// tray's own device view: if the agent has announced a countdown, dry run
/// included, that is the state that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookState {
    Mains,
    OnBattery,
    Pending,
}

/// Fold the tray's power view and the agent's pending flag into a state, or
/// `None` when nothing is known. Unknown is not a power state: the lights
/// keep whatever they were, symmetrical with the notifications refusing to
/// announce recoveries they never saw.
pub fn state_of(power: Power, pending: bool) -> Option<HookState> {
    if pending {
        return Some(HookState::Pending);
    }
    match power {
        Power::Mains => Some(HookState::Mains),
        Power::Battery | Power::Critical => Some(HookState::OnBattery),
        Power::Unknown => None,
    }
}

/// The change detector. At most one hook per actual change of state.
#[derive(Debug, Default)]
pub struct Hooks {
    last: Option<HookState>,
}

impl Hooks {
    /// Fold in the current state and say which hook fires, if any.
    ///
    /// The first known state fires `OnBattery` and `Pending` but never
    /// `Mains`: a tray starting mid-outage should light the room up, but a
    /// tray starting at logon on healthy mains must not reset lighting the
    /// user set themselves. Same asymmetry as the startup toasts.
    pub fn observe(&mut self, state: Option<HookState>) -> Option<HookState> {
        let now = state?;
        let prev = self.last.replace(now);
        match prev {
            Some(p) if p == now => None,
            Some(_) => Some(now),
            None => (now != HookState::Mains).then_some(now),
        }
    }
}

/// The configured command lines, owned by the tray.
#[derive(Debug, Default)]
pub struct Commands {
    on_battery: Option<String>,
    on_pending: Option<String>,
    on_mains: Option<String>,
}

impl Commands {
    pub fn from_settings(s: &Settings) -> Commands {
        Commands {
            on_battery: s.on_battery_cmd.clone(),
            on_pending: s.on_pending_cmd.clone(),
            on_mains: s.on_mains_cmd.clone(),
        }
    }

    pub fn for_state(&self, state: HookState) -> Option<&str> {
        match state {
            HookState::Mains => self.on_mains.as_deref(),
            HookState::OnBattery => self.on_battery.as_deref(),
            HookState::Pending => self.on_pending.as_deref(),
        }
    }
}

/// Fire and forget, with no console window.
///
/// Through `cmd /C` so the line gets ordinary shell semantics -- PATH lookup,
/// quoting, whatever the user wrote -- and via `raw_arg` so nothing re-quotes
/// it on the way. The tray is a windows-subsystem binary, so without
/// CREATE_NO_WINDOW every hook would flash a console over the user's screen
/// at the exact moment the power failed.
pub fn run(cmd: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(cmd)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_outranks_the_device_view() {
        assert_eq!(state_of(Power::Mains, true), Some(HookState::Pending));
        assert_eq!(state_of(Power::Battery, true), Some(HookState::Pending));
        assert_eq!(state_of(Power::Unknown, true), Some(HookState::Pending));
    }

    #[test]
    fn unknown_is_not_a_state_and_holds_the_previous_one() {
        let mut h = Hooks::default();
        assert_eq!(h.observe(state_of(Power::Battery, false)), Some(HookState::OnBattery));
        // The UPS drops out of sight mid-outage. Nothing fires, and when it
        // comes back still on battery, nothing fires either: the room is
        // already amber.
        assert_eq!(h.observe(state_of(Power::Unknown, false)), None);
        assert_eq!(h.observe(state_of(Power::Battery, false)), None);
    }

    #[test]
    fn a_change_fires_exactly_once() {
        let mut h = Hooks::default();
        h.observe(state_of(Power::Mains, false));
        assert_eq!(h.observe(state_of(Power::Battery, false)), Some(HookState::OnBattery));
        assert_eq!(h.observe(state_of(Power::Battery, false)), None, "re-fired without a change");
        assert_eq!(h.observe(state_of(Power::Mains, false)), Some(HookState::Mains));
        assert_eq!(h.observe(state_of(Power::Mains, false)), None);
    }

    /// A tray starting mid-outage lights the room up; a tray starting at logon
    /// on healthy mains must not reset lighting the user set themselves.
    #[test]
    fn startup_lights_up_but_never_resets() {
        let mut fresh = Hooks::default();
        assert_eq!(fresh.observe(state_of(Power::Mains, false)), None, "reset the room at logon");
        // ...and having adopted mains silently, a later outage still fires.
        assert_eq!(fresh.observe(state_of(Power::Battery, false)), Some(HookState::OnBattery));

        let mut mid_outage = Hooks::default();
        assert_eq!(mid_outage.observe(state_of(Power::Battery, false)), Some(HookState::OnBattery));

        let mut mid_countdown = Hooks::default();
        assert_eq!(mid_countdown.observe(state_of(Power::Battery, true)), Some(HookState::Pending));
    }

    /// A countdown cancelled while still on battery -- a wake retraction, not
    /// a recovery -- falls back to amber, not to normal.
    #[test]
    fn a_cancellation_on_battery_falls_back_to_battery() {
        let mut h = Hooks::default();
        h.observe(state_of(Power::Mains, false));
        h.observe(state_of(Power::Battery, false));
        assert_eq!(h.observe(state_of(Power::Battery, true)), Some(HookState::Pending));
        assert_eq!(h.observe(state_of(Power::Battery, false)), Some(HookState::OnBattery));
        // And one cancelled by the power coming back goes straight to normal.
        assert_eq!(h.observe(state_of(Power::Mains, false)), Some(HookState::Mains));
    }

    #[test]
    fn the_commands_map_one_to_one() {
        let s = Settings {
            on_battery_cmd: Some("amber".into()),
            on_pending_cmd: Some("red".into()),
            ..Settings::default()
        };
        let c = Commands::from_settings(&s);
        assert_eq!(c.for_state(HookState::OnBattery), Some("amber"));
        assert_eq!(c.for_state(HookState::Pending), Some("red"));
        assert_eq!(c.for_state(HookState::Mains), None, "an unset hook ran something");
    }
}
