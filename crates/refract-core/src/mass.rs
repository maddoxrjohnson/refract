//! How heavy a repo is, which sets the size of its gem (spec §3.2).
//!
//! Mass blends the repo's tracked size with its commits in the last 30 days. Both are
//! log-scaled, so a 1 GB repo is not a thousand times heavier than a 1 MB one, then
//! each is divided by its largest value across all repos. The blend is finally divided
//! by the largest blend, so the heaviest repo has mass 1.

/// Share of the mass that comes from tracked size; the rest comes from recent commits.
pub const SIZE_WEIGHT: f64 = 0.5;

/// What mass is computed from, for one repo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MassInput {
    /// Total size of tracked files in bytes.
    pub tracked_bytes: u64,
    /// Commits reachable from HEAD made in the last 30 days.
    pub commits_30d: u32,
}

/// Returns each repo's mass in 0..=1, in input order. All-zero inputs give all zeros.
pub fn masses(inputs: &[MassInput]) -> Vec<f64> {
    let size: Vec<f64> = inputs.iter().map(|i| log1p(i.tracked_bytes)).collect();
    let commits: Vec<f64> = inputs.iter().map(|i| log1p(i.commits_30d.into())).collect();
    let size = scale_to_max(&size);
    let commits = scale_to_max(&commits);
    let blend: Vec<f64> = size
        .iter()
        .zip(&commits)
        .map(|(s, c)| SIZE_WEIGHT * s + (1.0 - SIZE_WEIGHT) * c)
        .collect();
    scale_to_max(&blend)
}

fn log1p(value: u64) -> f64 {
    (value as f64).ln_1p()
}

/// Divides every value by the largest one; leaves all zeros when the largest is zero.
fn scale_to_max(values: &[f64]) -> Vec<f64> {
    let max = values.iter().copied().fold(0.0, f64::max);
    if max <= 0.0 {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| v / max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tracked_bytes: u64, commits_30d: u32) -> MassInput {
        MassInput {
            tracked_bytes,
            commits_30d,
        }
    }

    #[test]
    fn empty_input_gives_empty_output() {
        assert!(masses(&[]).is_empty());
    }

    #[test]
    fn single_repo_has_full_mass() {
        assert_eq!(masses(&[input(5_000, 3)]), vec![1.0]);
        assert_eq!(masses(&[input(5_000, 0)]), vec![1.0]);
    }

    #[test]
    fn all_zero_repos_have_zero_mass() {
        assert_eq!(masses(&[input(0, 0), input(0, 0)]), vec![0.0, 0.0]);
        assert_eq!(masses(&[input(0, 0)]), vec![0.0]);
    }

    #[test]
    fn values_stay_in_unit_range_and_heaviest_is_one() {
        let m = masses(&[input(10, 0), input(1 << 30, 200), input(4096, 7)]);
        assert!(m.iter().all(|v| (0.0..=1.0).contains(v)));
        assert_eq!(m[1], 1.0);
    }

    #[test]
    fn mass_grows_with_size_and_with_commits() {
        let by_size = masses(&[input(1_000, 5), input(100_000, 5), input(10_000_000, 5)]);
        assert!(by_size[0] < by_size[1] && by_size[1] < by_size[2]);
        let by_commits = masses(&[input(1_000, 0), input(1_000, 10), input(1_000, 100)]);
        assert!(by_commits[0] < by_commits[1] && by_commits[1] < by_commits[2]);
    }

    #[test]
    fn log_scaling_keeps_small_repos_visible() {
        let m = masses(&[input(1_000_000_000, 0), input(1_000_000, 0)]);
        assert!(
            m[1] > 0.5,
            "1 MB next to 1 GB should keep most of its mass: {m:?}"
        );
    }

    #[test]
    fn recent_activity_can_outweigh_size() {
        let m = masses(&[input(50_000_000, 0), input(200_000, 60)]);
        assert!(m[1] > m[0], "{m:?}");
    }
}
