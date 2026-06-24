# Proposal: per-group enemy AI (replace army-level orders)

**Status:** proposal (not yet implemented)
**Area:** AI / movement-orders (`sim_core`)
**Author:** autonomous engineering run, 2026-06-24
**Why this is a doc and not code:** see [Run blocker](#run-blocker) — this
session could not compile or run `cargo test -p sim_core`, so it does not push
untested simulation logic. This document specifies the change precisely enough
that the next run (or a human) with a working crate registry can implement and
test it directly.

---

## Run blocker (why no code change this run)

The remote execution environment's egress policy blocks every crates.io tarball
host, and the local cargo cache is empty, so the dependency tree (bevy_ecs,
hexx, …) cannot be downloaded and **`cargo test -p sim_core` cannot run**.

Verified this run:

| Host | Reachable? | Role |
|------|-----------|------|
| `index.crates.io` | ✅ 200 | sparse index (metadata only) |
| `github.com` | ✅ 200 | git |
| `static.crates.io` | ❌ 403 / blocked | **crate tarball downloads** |
| `crates.io` | ❌ blocked | api / download redirect |
| `rsproxy.cn`, `mirrors.tuna…`, `mirrors.ustc…`, S3/CDN mirrors | ❌ blocked | alternative crate mirrors |

`cargo fetch` fails with `[56] CONNECT tunnel failed, response 403` on
`static.crates.io`; `cargo fetch --offline` fails because nothing is cached
(e.g. `failed to download tokio v1.52.3`). This is an **organization egress
policy denial**, not a TLS/CA misconfiguration (`CARGO_HTTP_CAINFO` is set
correctly and `index.crates.io` resolves fine), so it cannot be worked around
from inside the session. The fix is environment-side: allow `static.crates.io`
(and `crates.io`) in the network policy, or pre-warm `~/.cargo/registry` in the
container setup script.

Because the project's hard rule is *every change must keep `cargo test -p
sim_core` green and add tests*, and untested edits to a deterministic
simulation are high-risk, this run delivers the prescribed fallback: a
documented analysis and a concrete, prioritized proposal.

---

## Problem

`enemy_ai` (`crates/sim_core/src/lib.rs`) drives the Blue army with a **single,
army-wide order**. It tallies *all* Blue, *all* Red, and *all* engaged Blue,
then writes one order to `(Team::Blue, 1)`:

```rust
pub fn enemy_ai(units: Query<(&Hex, &Team)>, idx: Res<SpatialIndex>, mut orders: ResMut<Orders>) {
    let (mut own, mut foe, mut engaged) = (0u32, 0u32, 0u32);
    for (hex, team) in &units {
        match team {
            Team::Blue => {
                own += 1;
                if hex.neighbors().iter().any(|n| matches!(idx.at(*n), Some((_, t)) if t == Team::Red)) {
                    engaged += 1;
                }
            }
            Team::Red => foe += 1,
        }
    }
    orders.set(Team::Blue, 1, ai_order(own, foe, engaged));
}
```

The `Orders` resource and every consumer (`combat`, `movement`,
`update_stamina`, `build_formations`) are **already keyed by `(Team, group)`**
via `Group(pub u8)` — the per-group machinery exists. Only the AI ignores it.
The README explicitly lists this as pending:

> *"IA enemiga más rica … órdenes por grupo en vez de army-level"*

### Consequence

One global balance erases local situation. A Blue flank that is being overrun
(0.3× locally) is told to `Charge` because the army-wide ratio is healthy; a
group that locally dominates can't `Unleash` while another part of the line is
losing. The whole army oscillates between one order. This both reads as
unintelligent and wastes the order set (`Retreat`/`Hold`/`Charge`/`Unleash`)
that earlier runs added.

---

## Proposed change

Compute the balance **per Blue group** and set each `(Blue, g)` independently.
`ai_order(own, foe, engaged)` is already a pure function of three counts and
needs **no change** — only the aggregation that feeds it.

"Local foe strength" should be the Red pressure *that group actually faces*, not
the global Red count. Two defensible definitions; the proposal picks (B) for a
first cut because it is O(units) and needs no extra spatial queries:

- **(A) proximity-weighted:** for each Blue unit, count Red within radius `K`
  (e.g. 3 hexes) — most accurate, but O(units · disk(K)) ring scans.
- **(B) contact-based (chosen):** `foe_g` = number of *distinct Red units
  adjacent to any unit of group g*. Already computable from the per-unit
  neighbor scan `enemy_ai` does today; a group with no contact uses the global
  Red count as a fallback so it still advances/holds sensibly before contact.

### Reference implementation (drop-in replacement for `enemy_ai`)

```rust
/// Sets Blue's orders each tick, independently per group, from the local
/// balance each group faces. `ai_order` is unchanged; only the aggregation is
/// per-group now.
pub fn enemy_ai(
    units: Query<(&Hex, &Team, &Group)>,
    idx: Res<SpatialIndex>,
    mut orders: ResMut<Orders>,
) {
    use std::collections::{HashMap, HashSet};

    let mut total_red = 0u32;
    // group -> (own count, engaged count, set of adjacent enemy hexes)
    let mut blue: HashMap<u8, (u32, u32, HashSet<Hex>)> = HashMap::new();

    for (hex, team, group) in &units {
        match team {
            Team::Red => total_red += 1,
            Team::Blue => {
                let e = blue.entry(group.0).or_default();
                e.0 += 1;
                let mut touched = false;
                for n in hex.neighbors() {
                    if matches!(idx.at(n), Some((_, t)) if t == Team::Red) {
                        touched = true;
                        e.2.insert(n); // dedupe: one enemy fought by two of us counts once
                    }
                }
                if touched {
                    e.1 += 1;
                }
            }
        }
    }

    for (g, (own, engaged, adj_enemies)) in blue {
        // Local foe = distinct adjacent Reds; before contact, fall back to the
        // global Red count so an un-engaged group still advances/holds sensibly.
        let foe = if adj_enemies.is_empty() {
            total_red
        } else {
            adj_enemies.len() as u32
        };
        orders.set(Team::Blue, g, ai_order(own, foe, engaged));
    }
}
```

Notes:
- `Hex` already derives `Hash + Eq` (verified in `lib.rs`), so `HashSet<Hex>`
  works directly — no tuple key or extra derive needed.
- Keeps `enemy_ai`'s single-pass O(units) cost (plus a tiny per-group map);
  no new `Res`/`Query` beyond adding `&Group`, which other systems already read.
- `ai_order`, the `Order` enum, and all downstream systems are untouched, so the
  blast radius is one function.

---

## Tests to add (must pass before merge)

These follow the existing headless `fresh_world()` + `step()` pattern in the
`tests` module. All are pure `sim_core`, no GPU.

1. **`per_group_orders_diverge_under_local_pressure`** — spawn two Blue groups:
   group 1 swarmed by many adjacent Red (local ≪ 0.5×), group 2 alone far from
   any Red. After one `enemy_ai` run assert
   `orders.get(Blue,1) == Order::Retreat` **and**
   `orders.get(Blue,2) == Order::March` — proving the two groups no longer share
   one order. (Today both would get the army-wide order.)

2. **`engaged_dominant_group_unleashes_independently`** — group 1: a few Blue
   each surrounded by Reds but globally Blue ≥ 1.25× locally and in contact →
   `Unleash`; a second isolated group stays `March`. Guards the dominant branch.

3. **`ungrouped_contact_falls_back_to_global_count`** — single Blue group with
   no adjacency to Red but Red present on the map → `foe == total_red` path →
   `March`/`Hold` as `ai_order` dictates (lock in the pre-contact fallback).

4. **Regression — keep existing `enemy_ai`-dependent assertions green.** The
   integration test `battle_resolves_to_a_decided_outcome` must still terminate
   in a decided outcome; per-group orders should not deadlock the battle.

5. **Determinism unchanged.** The existing bit-for-bit determinism property test
   must stay green; iterating a `HashMap` for *side-effect-free* `orders.set`
   calls is order-independent (each writes a distinct key), so determinism holds.
   If a future reviewer wants belt-and-suspenders, collect into a `BTreeMap`
   before the `set` loop.

---

## Risks & trade-offs

- **Group assignment is the real lever.** With one Blue group today, this change
  is a no-op behaviorally until the spawner (`crates/imperium/src/main.rs`)
  assigns Blue units to several groups. The proposal is still worth landing in
  `sim_core` first (with multi-group tests that construct groups directly), so
  the engine is ready; the render-crate spawn change is a separate, small follow
  up.
- **Contact-based foe metric is coarse** (only counts *adjacent* Reds). It is
  intentionally the cheap first cut; option (A) proximity weighting is the
  natural follow-up if groups dither at the moment of contact.
- **No fragmentation/merge logic.** Groups are static; this proposal does not
  add dynamic regrouping. Out of scope by design (one focused change).

---

## Prioritized backlog (other candidates considered this run)

Ranked by value-to-risk for a single focused, testable `sim_core` change:

1. **Per-group enemy AI** (this doc) — unlocks the existing order set; small
   blast radius; pure and testable. **Recommended next.**
2. **Proximity-weighted local balance (option A)** — natural refinement once (1)
   lands; needs a radius-`K` ring scan reusing the dense `SpatialIndex`.
3. **Dynamic group regrouping** — split an encircled group, merge remnants;
   higher complexity, needs careful determinism tests.
4. **A\* recompute budgeting** — cap path recomputations per tick across the army
   (a global budget on top of the existing per-unit `PathCache`) for the
   tens-of-thousands target; needs a stress benchmark to justify.

---

*Environment note for whoever picks this up: confirm `cargo test -p sim_core`
runs (the registry must be reachable). If you hit the same egress block, ask the
environment owner to allowlist `static.crates.io` or pre-warm
`~/.cargo/registry` in the setup script — the code change above is ready to type
once the toolchain can build.*
