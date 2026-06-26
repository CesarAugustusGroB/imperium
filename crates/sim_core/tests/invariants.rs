//! Pure-logic invariant tests for `sim_core`, kept as an integration test so
//! they exercise only the crate's public API (the AI balance function and the
//! terrain generator). These pin down two contracts the README leans on — the
//! AI's "amass → launch / defend-when-losing" threshold ladder and the
//! deterministic seeded terrain — that previously had only indirect, end-to-end
//! coverage. No production code depends on this file.

use sim_core::*;

/// Aggression rank for an *in-contact* order (engaged > 0), least to most
/// committed. With units engaged, `ai_order` only ever yields these four —
/// March is reserved for the not-yet-in-contact case — so anything else here is
/// a logic bug.
fn engaged_rank(order: Order) -> u8 {
    match order {
        Order::Retreat => 0,
        Order::Hold => 1,
        Order::Charge => 2,
        Order::Unleash => 3,
        other => panic!("engaged AI should never yield {other:?}"),
    }
}

#[test]
fn ai_order_decision_table_matches_documented_thresholds() {
    // No enemy left → always advance, whatever the count or contact.
    assert_eq!(ai_order(0, 0, 0), Order::March);
    assert_eq!(ai_order(50, 0, 7), Order::March);

    // Routed: strictly under 0.5× → Retreat, in or out of contact. This is the
    // only ai_order-level coverage of the Retreat branch.
    assert_eq!(ai_order(4, 10, 0), Order::Retreat, "< 0.5x out of contact");
    assert_eq!(ai_order(4, 10, 3), Order::Retreat, "< 0.5x in contact");
    // Exactly 0.5× is NOT routed (the cliff is strict): it holds.
    assert_eq!(ai_order(5, 10, 0), Order::Hold, "exactly 0.5x holds, not retreats");

    // Outnumbered band [0.5x, 0.8x) → Hold regardless of contact.
    assert_eq!(ai_order(7, 10, 0), Order::Hold, "0.7x out of contact");
    assert_eq!(ai_order(7, 10, 4), Order::Hold, "0.7x in contact");

    // Even-to-strong but NOT in contact → still just March (amass, then launch).
    assert_eq!(ai_order(10, 10, 0), Order::March, "even but no contact → march");
    assert_eq!(ai_order(100, 10, 0), Order::March, "dominant but no contact → march");

    // In contact, roughly even [0.8x, 1.0x) → Hold the line.
    assert_eq!(ai_order(9, 10, 1), Order::Hold, "0.9x engaged → hold");

    // In contact and not behind [1.0x, 1.25x) → Charge.
    assert_eq!(ai_order(10, 10, 1), Order::Charge, "even & engaged → charge");
    assert_eq!(ai_order(11, 10, 1), Order::Charge, "1.1x engaged → charge");

    // In contact and dominant (≥ 1.25x) → Unleash. 5:4 is exactly 1.25×.
    assert_eq!(ai_order(5, 4, 1), Order::Unleash, "exactly 1.25x engaged → unleash");
    assert_eq!(ai_order(100, 10, 1), Order::Unleash, "crushing & engaged → unleash");
}

#[test]
fn ai_order_aggression_is_monotonic_in_strength_when_engaged() {
    // Property: with the foe count and contact fixed (and in contact), growing
    // our own numbers must never make the AI *less* aggressive. This guards the
    // threshold ladder against an inversion that a handful of point-checks could
    // miss.
    let foe = 100;
    let mut prev = engaged_rank(ai_order(1, foe, 1));
    for own in 1..=400u32 {
        let rank = engaged_rank(ai_order(own, foe, 1));
        assert!(
            rank >= prev,
            "aggression dropped as strength rose: own={own} gave rank {rank} after {prev}"
        );
        prev = rank;
    }
    // End points: vastly outnumbered routs, crushing superiority unleashes.
    assert_eq!(ai_order(1, foe, 1), Order::Retreat);
    assert_eq!(ai_order(400, foe, 1), Order::Unleash);
}

#[test]
fn generated_terrain_is_deterministic_and_total() {
    let (seed, qr, rr) = (12345, 6, 5);
    let a = generate_terrain(seed, qr, rr);
    let b = generate_terrain(seed, qr, rr);

    // Determinism: the same seed reproduces the map cell-for-cell.
    assert_eq!(a.tiles, b.tiles, "same seed must reproduce the same terrain");

    // Totality: every cell in the inclusive rectangle is present (no holes) and
    // is one of the five known biomes.
    let mut count = 0;
    for q in -qr..=qr {
        for r in -rr..=rr {
            let t = a.tiles.get(&(q, r)).copied();
            assert!(t.is_some(), "cell ({q},{r}) must be generated");
            assert!(
                matches!(
                    t.unwrap(),
                    Terrain::Plains
                        | Terrain::Forest
                        | Terrain::Hill
                        | Terrain::Mountain
                        | Terrain::Water
                ),
                "cell ({q},{r}) has an unknown biome",
            );
            count += 1;
        }
    }
    assert_eq!(count, (2 * qr + 1) * (2 * rr + 1), "exactly the rectangle, no more");
}

#[test]
fn different_seeds_produce_different_terrain() {
    // Distinct seeds must diverge somewhere over a reasonable field (the whole
    // point of the seed). Not a strict guarantee in general, but these two
    // concrete seeds do differ — a regression that collapsed the hash to ignore
    // the seed would trip this.
    let a = generate_terrain(1, 8, 8);
    let b = generate_terrain(2, 8, 8);
    assert_ne!(a.tiles, b.tiles, "two different seeds should not yield identical maps");
}

#[test]
fn terrain_map_defaults_unset_cells_to_plains() {
    // `get` outside the generated rectangle (or on an empty map) yields Plains,
    // the safe passable default the movement/line-of-sight code relies on.
    let map = generate_terrain(7, 2, 2);
    assert_eq!(map.get(Hex::new(1000, -1000)), Terrain::Plains, "off-map is Plains");
    assert_eq!(TerrainMap::default().get(Hex::new(0, 0)), Terrain::Plains, "empty map is Plains");
}
