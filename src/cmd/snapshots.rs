use super::update::prefetch_installed;
use crate::Error;
use crate::config::Config;
use crate::git::GitStore;
use crate::github::GithubApi;
use crate::inventory::Inventory;
use crate::lock::LockSlot;
use crate::origin::{self};
use crate::prune;
use crate::snapshot::{self, RefreshPlan, Snapshots, TapPlan};
use std::io::Write;
use std::path::Path;

pub fn refresh_plan(cfg: &Config, inv: &Inventory, extra: &[(String, String)]) -> RefreshPlan {
    RefreshPlan {
        hours: cfg.hours,
        core_hours: cfg.effective_hours(origin::CORE),
        cask_hours: cfg.effective_hours(origin::CASK),
        taps: inv
            .needed_taps(extra, cfg)
            .into_iter()
            .map(|(name, remote)| TapPlan {
                hours: cfg.effective_hours(&name),
                name,
                remote,
            })
            .collect(),
    }
}

/// The planned taps whose stored snapshot is missing or uses another window.
pub(super) fn stale_taps(stored: &Snapshots, plan: &RefreshPlan) -> Vec<TapPlan> {
    plan.taps
        .iter()
        .filter(|t| snapshot::tap_needs_refresh(stored, &t.name, t.hours))
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn ensure_snapshots(
    lock: &mut LockSlot,
    git: &impl GitStore,
    gh: &impl GithubApi,
    cache: &Path,
    cfg: &Config,
    inv: &Inventory,
    extra: &[(String, String)],
    now: time::OffsetDateTime,
    force: bool,
    verbose: bool,
    progress: &mut impl Write,
) -> Result<Snapshots, Error> {
    let plan = refresh_plan(cfg, inv, extra);
    let snaps = if force {
        let held = lock.get()?;
        snapshot::refresh_with(held, git, gh, cache, &plan, now, progress)?
    } else {
        // Decide from state.toml, take the lock if that means writing the
        // cache, then decide again: another run may have refreshed while this
        // one waited. Without the lock the read-only path never writes.
        let stored = loop {
            let stored = snapshot::load_state(cache);
            let needs_refresh = match &stored {
                Ok(Some(s)) => !stale_taps(s, &plan).is_empty(),
                Ok(None) | Err(_) => true,
            };
            if needs_refresh && !lock.is_held() {
                lock.get()?;
                continue;
            }
            break stored?;
        };
        match stored {
            Some(mut s) => {
                // outdated/info reuse the stored snapshot, but refresh any
                // tap with no state or a different soak window.
                let stale = stale_taps(&s, &plan);
                if !stale.is_empty() {
                    let held = lock.get()?;
                    snapshot::refresh_taps(held, git, cache, &mut s, &stale, now, progress)?;
                }
                return Ok(s);
            }
            None => {
                let held = lock.get()?;
                snapshot::refresh_with(held, git, gh, cache, &plan, now, progress)?
            }
        }
    };
    prune::prune_cache(lock.get()?, cache, inv, verbose, progress)?;
    prefetch_installed(git, cache, inv, &snaps);
    Ok(snaps)
}
