use crate::{Error, SoakHours};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub committer_time: OffsetDateTime,
}

pub trait GithubApi {
    fn head_sha(&self, repo: &str) -> Result<String, Error>;
    fn latest_commit_until(&self, repo: &str, until: OffsetDateTime) -> Result<CommitInfo, Error>;
}

pub struct UreqGithub {
    pub base: String, // "https://api.github.com"
}

pub struct StaticGithub {
    pub head: String,
    pub commits: Vec<CommitInfo>, // newest first
}

pub fn cutoff_instant(now: OffsetDateTime, hours: SoakHours) -> OffsetDateTime {
    now - Duration::hours(i64::from(hours.get()))
}

impl GithubApi for StaticGithub {
    fn head_sha(&self, _repo: &str) -> Result<String, Error> {
        Ok(self.head.clone())
    }

    fn latest_commit_until(&self, _repo: &str, until: OffsetDateTime) -> Result<CommitInfo, Error> {
        self.commits
            .iter()
            .find(|c| c.committer_time <= until)
            .cloned()
            .ok_or_else(|| Error::Other("no commit at or before cutoff".into()))
    }
}

impl UreqGithub {
    fn get_first_commit(
        &self,
        repo: &str,
        until: Option<OffsetDateTime>,
    ) -> Result<CommitInfo, Error> {
        let url = format!("{}/repos/{}/commits", self.base, repo);
        let mut req = ureq::get(&url).set("User-Agent", "brewsoak");
        if let Some(until) = until {
            let until_s = until
                .format(&Rfc3339)
                .map_err(|e| Error::Other(e.to_string()))?;
            req = req.query("until", &until_s);
        }
        req = req.query("per_page", "1");
        match req.call() {
            Ok(resp) => {
                let body = resp
                    .into_string()
                    .map_err(|e| Error::Other(e.to_string()))?;
                parse_first_commit(&body)
            }
            Err(ureq::Error::Status(code, _)) => Err(Error::Other(format!("github HTTP {code}"))),
            Err(e) => Err(Error::Other(format!("github: {e}"))),
        }
    }
}

impl GithubApi for UreqGithub {
    fn head_sha(&self, repo: &str) -> Result<String, Error> {
        Ok(self.get_first_commit(repo, None)?.sha)
    }

    fn latest_commit_until(&self, repo: &str, until: OffsetDateTime) -> Result<CommitInfo, Error> {
        self.get_first_commit(repo, Some(until))
    }
}

fn json_string_after<'a>(haystack: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let after = haystack.split_once(&pat)?.1.trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let after = after.strip_prefix('"')?;
    after.split_once('"').map(|(v, _)| v)
}

fn parse_first_commit(body: &str) -> Result<CommitInfo, Error> {
    let sha = json_string_after(body, "sha")
        .ok_or_else(|| Error::Other("github json missing sha".into()))?
        .to_string();
    let after_committer = body
        .split_once("\"committer\"")
        .map(|(_, rest)| rest)
        .ok_or_else(|| Error::Other("github json missing commit.committer".into()))?;
    let date = json_string_after(after_committer, "date")
        .ok_or_else(|| Error::Other("github json missing commit.committer.date".into()))?;
    let committer_time = OffsetDateTime::parse(date, &Rfc3339)
        .map_err(|e| Error::Other(format!("github committer date: {e}")))?;
    Ok(CommitInfo {
        sha,
        committer_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed now")
    }

    #[test]
    fn latest_commit_until_picks_pre_soak_commit() {
        let now = now();
        let gh = StaticGithub {
            head: "bb".into(),
            commits: vec![
                CommitInfo {
                    sha: "bb".into(),
                    committer_time: now - Duration::hours(2),
                },
                CommitInfo {
                    sha: "aa".into(),
                    committer_time: now - Duration::hours(30),
                },
            ],
        };
        let got = gh
            .latest_commit_until("Homebrew/homebrew-core", now - Duration::hours(24))
            .expect("cutoff commit");
        assert_eq!(got.sha, "aa");
    }

    #[test]
    fn cutoff_instant_subtracts_exactly_the_hours() {
        let now = now();
        let hours = SoakHours::new(24).expect("hours >= 1");
        assert_eq!(cutoff_instant(now, hours), now - Duration::hours(24));
    }
}
