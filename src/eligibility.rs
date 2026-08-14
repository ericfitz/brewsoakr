use crate::identity::{self, PkgIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Eligible,
    TooNew,
    Yanked,
    Deprecated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredAction {
    NoOpAlreadySoaked,
    LeaveAheadOfSoak,
    InstallCutoff,
    RefuseTooNew,
    RefuseYanked,
    RefuseDeprecated,
}

/// `cutoff_blob` / `head_blob` are raw git bytes. `today` is `YYYY-MM-DD`.
/// `installed` is parsed receipt identity.
pub fn upstream_status(
    cutoff_blob: Option<&[u8]>,
    head_blob: Option<&[u8]>,
    today: &str,
) -> UpstreamStatus {
    let Some(head) = head_blob else {
        return UpstreamStatus::Yanked;
    };
    let deprecated =
        std::str::from_utf8(head).is_ok_and(|rb| identity::is_deprecated_or_disabled(rb, today));
    if deprecated {
        UpstreamStatus::Deprecated
    } else if cutoff_blob.is_none() {
        UpstreamStatus::TooNew
    } else {
        UpstreamStatus::Eligible
    }
}

pub fn desired_action(
    status: UpstreamStatus,
    installed: Option<&PkgIdentity>,
    cutoff_id: Option<&PkgIdentity>,
    head_id: Option<&PkgIdentity>,
) -> DesiredAction {
    match status {
        UpstreamStatus::Yanked => DesiredAction::RefuseYanked,
        UpstreamStatus::Deprecated => DesiredAction::RefuseDeprecated,
        UpstreamStatus::TooNew => DesiredAction::RefuseTooNew,
        UpstreamStatus::Eligible => {
            if installed.is_none() {
                DesiredAction::InstallCutoff
            } else if identities_match(installed, cutoff_id) {
                DesiredAction::NoOpAlreadySoaked
            } else if identities_match(installed, head_id) && cutoff_id != head_id {
                DesiredAction::LeaveAheadOfSoak
            } else {
                DesiredAction::InstallCutoff
            }
        }
    }
}

fn identities_match(a: Option<&PkgIdentity>, b: Option<&PkgIdentity>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_artifact(b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{PkgIdentity, parse_formula};

    const CUTOFF_RB: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
  revision 1
  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
"#;

    const HEAD_RB: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "zzz999"
  revision 1
  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
"#;

    const OLD_BOTTLE_RB: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "old000"
  revision 1
  bottle do
    rebuild 1
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
"#;

    const DEPRECATED_HEAD_RB: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "zzz999"
  deprecate! date: "2024-01-01", because: :unmaintained
end
"#;

    const TODAY: &str = "2026-08-13";

    fn formula_id(rb: &str) -> PkgIdentity {
        PkgIdentity::Formula(parse_formula(rb).unwrap())
    }

    #[test]
    fn too_new() {
        let head = formula_id(HEAD_RB);
        let status = upstream_status(None, Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::TooNew);
        assert_eq!(
            desired_action(status, None, None, Some(&head)),
            DesiredAction::RefuseTooNew
        );
    }

    #[test]
    fn yanked() {
        let cutoff = formula_id(CUTOFF_RB);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), None, TODAY);
        assert_eq!(status, UpstreamStatus::Yanked);
        assert_eq!(
            desired_action(status, None, Some(&cutoff), None),
            DesiredAction::RefuseYanked
        );
    }

    #[test]
    fn deprecated() {
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(DEPRECATED_HEAD_RB);
        let status = upstream_status(
            Some(CUTOFF_RB.as_bytes()),
            Some(DEPRECATED_HEAD_RB.as_bytes()),
            TODAY,
        );
        assert_eq!(status, UpstreamStatus::Deprecated);
        assert_eq!(
            desired_action(status, None, Some(&cutoff), Some(&head)),
            DesiredAction::RefuseDeprecated
        );
    }

    #[test]
    fn already_soaked() {
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(HEAD_RB);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::Eligible);
        assert_eq!(
            desired_action(status, Some(&cutoff), Some(&cutoff), Some(&head)),
            DesiredAction::NoOpAlreadySoaked
        );
    }

    #[test]
    fn receipt_without_bottle_matches_cutoff_rebuild() {
        // Homebrew Cellar receipts omit the bottle block. Missing rebuild is
        // not rebuild 0; version/revision/sha256 still identify the artifact.
        const RECEIPT_RB: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
  revision 1
end
"#;
        let installed = formula_id(RECEIPT_RB);
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(HEAD_RB);
        assert_ne!(
            installed, cutoff,
            "exact identity still sees missing rebuild"
        );
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(
            desired_action(status, Some(&installed), Some(&cutoff), Some(&head)),
            DesiredAction::NoOpAlreadySoaked
        );
    }

    #[test]
    fn ahead_of_soak() {
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(HEAD_RB);
        assert_eq!(
            match (&cutoff, &head) {
                (PkgIdentity::Formula(c), PkgIdentity::Formula(h)) =>
                    (c.version.as_str(), h.version.as_str()),
                _ => unreachable!(),
            },
            ("1.21.4", "1.21.4")
        );
        assert_ne!(cutoff, head);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::Eligible);
        assert_eq!(
            desired_action(status, Some(&head), Some(&cutoff), Some(&head)),
            DesiredAction::LeaveAheadOfSoak
        );
    }

    #[test]
    fn behind_soak_other_identity() {
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(HEAD_RB);
        let installed = formula_id(OLD_BOTTLE_RB);
        assert_eq!(
            match (&cutoff, &head, &installed) {
                (PkgIdentity::Formula(c), PkgIdentity::Formula(h), PkgIdentity::Formula(i)) =>
                    (c.version.as_str(), h.version.as_str(), i.version.as_str()),
                _ => unreachable!(),
            },
            ("1.21.4", "1.21.4", "1.21.4")
        );
        assert_ne!(installed, cutoff);
        assert_ne!(installed, head);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::Eligible);
        assert_eq!(
            desired_action(status, Some(&installed), Some(&cutoff), Some(&head)),
            DesiredAction::InstallCutoff
        );
    }

    #[test]
    fn future_deprecate_date_is_eligible() {
        let rb = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "zzz999"
  deprecate! date: "2030-11-01", because: :deprecated_upstream
end
"#;
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(rb);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(rb.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::Eligible);
        assert_eq!(
            desired_action(status, None, Some(&cutoff), Some(&head)),
            DesiredAction::InstallCutoff
        );
    }

    #[test]
    fn not_installed_eligible() {
        let cutoff = formula_id(CUTOFF_RB);
        let head = formula_id(HEAD_RB);
        let status = upstream_status(Some(CUTOFF_RB.as_bytes()), Some(HEAD_RB.as_bytes()), TODAY);
        assert_eq!(status, UpstreamStatus::Eligible);
        assert_eq!(
            desired_action(status, None, Some(&cutoff), Some(&head)),
            DesiredAction::InstallCutoff
        );
    }
}
