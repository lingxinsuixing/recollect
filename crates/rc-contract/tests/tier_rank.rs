use rc_contract::TurnTier;

#[test]
fn the_rank_mapping_is_a_bijection_on_the_three_tiers() {
    assert_eq!(TurnTier::from_rank(TurnTier::IdBearing.rank()), Some(TurnTier::IdBearing));
    assert_eq!(TurnTier::from_rank(TurnTier::StableSeq.rank()), Some(TurnTier::StableSeq));
    assert_eq!(TurnTier::from_rank(TurnTier::ContentOnly.rank()), Some(TurnTier::ContentOnly));
}

#[test]
fn an_out_of_range_rank_has_no_tier() {
    assert_eq!(TurnTier::from_rank(0), None);
    assert_eq!(TurnTier::from_rank(4), None);
    assert_eq!(TurnTier::from_rank(255), None);
}

#[test]
fn every_tier_round_trips_through_its_rank() {
    for t in [TurnTier::IdBearing, TurnTier::StableSeq, TurnTier::ContentOnly] {
        assert_eq!(TurnTier::from_rank(t.rank()), Some(t));
    }
}
