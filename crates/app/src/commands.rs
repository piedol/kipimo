//! Every button and menu item carries a `Command` as its tag and sends
//! `command:` to the shared controller, which routes here.

use crate::tools_window::Tab;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    TogglePanel,
    OpenTools(Tab),
    OpenSettings,
    OpenTrash,
    /// The recommendation card's button.
    Recommendation,
    /// The recommendation card's "Later".
    SnoozeRecommendation,
    /// The panel's gear.
    GearMenu,
    ToggleLogin,
    /// Check for Updates…, Update to Kipimo X.Y.Z…, or the install under way.
    Update,
    /// A button in the update window or an update message (see `update_ui`).
    UpdateButton(u8),
    Quit,
    /// A Settings window control.
    Setting(u16),
}

impl Command {
    pub fn tag(self) -> isize {
        use Command::*;
        match self {
            TogglePanel => 1,
            OpenSettings => 2,
            OpenTrash => 3,
            Recommendation => 4,
            SnoozeRecommendation => 5,
            GearMenu => 6,
            ToggleLogin => 7,
            Update => 8,
            Quit => 9,
            UpdateButton(b) => 20 + b.min(9) as isize,
            OpenTools(t) => 100 + t.index() as isize,
            Setting(i) => 1000 + i as isize,
        }
    }

    pub fn from_tag(tag: isize) -> Option<Command> {
        use Command::*;
        Some(match tag {
            1 => TogglePanel,
            2 => OpenSettings,
            3 => OpenTrash,
            4 => Recommendation,
            5 => SnoozeRecommendation,
            6 => GearMenu,
            7 => ToggleLogin,
            8 => Update,
            9 => Quit,
            20..=29 => UpdateButton((tag - 20) as u8),
            100..=199 => OpenTools(*Tab::ALL.get((tag - 100) as usize)?),
            1000..=1999 => Setting((tag - 1000) as u16),
            _ => return None,
        })
    }
}

/// Route a command to the module that owns it.
pub fn run(cmd: Command) {
    crate::util::log(format!("command {cmd:?}"));
    crate::app::handle(cmd);
}

#[cfg(test)]
mod tests {
    use super::Command::*;
    use super::*;

    #[test]
    fn tags_round_trip() {
        let mut all = vec![
            TogglePanel,
            OpenSettings,
            OpenTrash,
            Recommendation,
            SnoozeRecommendation,
            GearMenu,
            ToggleLogin,
            Update,
            UpdateButton(0),
            UpdateButton(9),
            Quit,
            Setting(0),
            Setting(42),
        ];
        all.extend(Tab::ALL.iter().map(|&t| OpenTools(t)));
        for c in all {
            assert_eq!(Command::from_tag(c.tag()), Some(c), "{c:?}");
        }
        assert_eq!(Command::from_tag(0), None);
        assert_eq!(Command::from_tag(150), None);
    }
}
