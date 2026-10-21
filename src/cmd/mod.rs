use crate::flags;
use crate::inventory::{self};

mod apply;
mod bare;
mod deps;
mod info;
mod install;
mod notes;
mod outdated;
mod snapshots;
#[cfg(test)]
mod test_support;
mod update;
mod upgrade;
mod view;

pub use apply::reinstall;
pub(crate) use apply::{already_installed_line, max_status, merge_status};
pub use info::info;
pub use install::install;
pub use notes::{newer_than_cutoff_note, refusal_message};
pub use outdated::outdated;
pub use snapshots::{ensure_snapshots, refresh_plan};
pub use update::{prefetch_installed, update};
pub use upgrade::upgrade;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub refused: bool,
    pub brew_status: Option<i32>,
}

pub fn combine_exit(refused: bool, brew_status: Option<i32>) -> i32 {
    let brew = brew_status.unwrap_or(0);
    if brew > 1 {
        brew
    } else if refused {
        1
    } else {
        brew
    }
}

/// `(origin, name)` of every `user/tap/name` token on the command line.
pub fn explicit_tap_tokens(names: &[String]) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|n| {
            let token = inventory::parse_token(n).ok()?;
            token.origin.map(|o| (o, token.name))
        })
        .collect()
}

pub fn is_verbose(flags: &[String]) -> bool {
    flags
        .iter()
        .any(|f| f == "--verbose" || (flags::is_short_cluster(f) && f[1..].contains('v')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_exit_refused_with_ok_brew_is_1() {
        assert_eq!(combine_exit(true, Some(0)), 1);
    }

    #[test]
    fn combine_exit_success_is_0() {
        assert_eq!(combine_exit(false, Some(0)), 0);
    }

    #[test]
    fn combine_exit_brew_gt_1_wins() {
        assert_eq!(combine_exit(true, Some(3)), 3);
    }
}
