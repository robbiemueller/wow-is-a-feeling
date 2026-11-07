//! Popularity tiers, per `docs/DESIGN.md`: top 10 legendary, next 5% epic, next
//! 15% rare, next 30% uncommon, the rest common. Grey is reserved for addons that
//! no longer target Forever. It's only a color, so a rough score is fine.

use std::collections::BTreeMap;

use crate::model::Candidate;

const TOP_LEGENDARY: usize = 10;

/// Sources count very differently, so everything is compared in orders of magnitude.
pub fn popularity(candidate: &Candidate, boost: f64) -> f64 {
    let scale = |n: u64| ((n + 1) as f64).log10();
    scale(candidate.addon.downloads)
        + 0.75 * scale(u64::from(candidate.addon.stars))
        + 0.5 * scale(candidate.favorites)
        + boost
}

/// Sets `tier` on every candidate. `boosts` is keyed by addon id.
pub fn assign_tiers(candidates: &mut [Candidate], boosts: &BTreeMap<String, f64>) {
    let mut ranked: Vec<(usize, f64)> = Vec::with_capacity(candidates.len());
    for (at, candidate) in candidates.iter_mut().enumerate() {
        if candidate.no_longer_forever {
            candidate.addon.tier = 0;
        } else if !candidate.addon.trusted {
            // Its counts are claims until it's established, so they earn no color.
            candidate.addon.tier = 1;
        } else {
            let boost = boosts.get(&candidate.addon.id).copied().unwrap_or(0.0);
            ranked.push((at, popularity(candidate, boost)));
        }
    }
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| candidates[a.0].addon.id.cmp(&candidates[b.0].addon.id))
    });

    let total = ranked.len();
    let share = |percent: usize| (total * percent).div_ceil(100);
    let epic_end = TOP_LEGENDARY + share(5);
    let rare_end = epic_end + share(15);
    let uncommon_end = rare_end + share(30);
    for (rank, (at, _)) in ranked.into_iter().enumerate() {
        candidates[at].addon.tier = if rank < TOP_LEGENDARY {
            5
        } else if rank < epic_end {
            4
        } else if rank < rare_end {
            3
        } else if rank < uncommon_end {
            2
        } else {
            1
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(count: usize) -> Vec<Candidate> {
        (0..count)
            .map(|i| {
                let mut c = Candidate::default();
                c.addon.id = format!("gh:a/{i:04}");
                c.addon.downloads = (count - i) as u64 * 100;
                c.addon.trusted = true;
                c
            })
            .collect()
    }

    #[test]
    fn an_untrusted_entry_earns_no_color_however_popular() {
        let mut list = candidates(100);
        list[0].addon.trusted = false;
        list[0].addon.downloads = u64::MAX / 2;
        assign_tiers(&mut list, &BTreeMap::new());
        assert_eq!(list[0].addon.tier, 1);
        assert_eq!(list[1].addon.tier, 5);
    }

    #[test]
    fn tiers_follow_the_design_shares() {
        let mut list = candidates(1000);
        assign_tiers(&mut list, &BTreeMap::new());
        let count = |tier: u8| list.iter().filter(|c| c.addon.tier == tier).count();
        assert_eq!(count(5), 10);
        assert_eq!(count(4), 50);
        assert_eq!(count(3), 150);
        assert_eq!(count(2), 300);
        assert_eq!(count(1), 490);
        assert_eq!(count(0), 0);
        assert_eq!(list[0].addon.tier, 5, "the most downloaded is legendary");
        assert_eq!(list[999].addon.tier, 1);
    }

    #[test]
    fn small_catalogs_do_not_break_the_arithmetic() {
        for size in [0, 1, 5, 10, 11, 12] {
            let mut list = candidates(size);
            assign_tiers(&mut list, &BTreeMap::new());
            assert!(list.iter().all(|c| (1..=5).contains(&c.addon.tier)));
        }
    }

    #[test]
    fn stars_and_favorites_count_and_boosts_lift() {
        let mut unknown = Candidate::default();
        unknown.addon.id = "a".into();
        let mut starred = Candidate::default();
        starred.addon.id = "b".into();
        starred.addon.stars = 1000;
        assert!(popularity(&starred, 0.0) > popularity(&unknown, 0.0));
        assert!(popularity(&unknown, 3.0) > popularity(&starred, 0.0));

        let mut list = candidates(100);
        let last = list[99].addon.id.clone();
        let boosts = BTreeMap::from([(last, 10.0)]);
        assign_tiers(&mut list, &boosts);
        assert_eq!(list[99].addon.tier, 5);
    }

    #[test]
    fn addons_that_dropped_forever_are_grey_and_do_not_take_a_slot() {
        let mut list = candidates(20);
        list[0].no_longer_forever = true;
        assign_tiers(&mut list, &BTreeMap::new());
        assert_eq!(list[0].addon.tier, 0);
        assert_eq!(list.iter().filter(|c| c.addon.tier == 5).count(), 10);
        assert_eq!(list[1].addon.tier, 5);
    }

    #[test]
    fn equal_scores_are_ordered_by_id_so_builds_are_stable() {
        let mut a = candidates(30);
        for c in &mut a {
            c.addon.downloads = 5;
        }
        let mut b = a.clone();
        b.reverse();
        assign_tiers(&mut a, &BTreeMap::new());
        assign_tiers(&mut b, &BTreeMap::new());
        b.reverse();
        assert_eq!(a, b);
    }
}
