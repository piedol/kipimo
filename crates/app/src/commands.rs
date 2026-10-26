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
            Quit => 9,
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
            9 => Quit,
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
