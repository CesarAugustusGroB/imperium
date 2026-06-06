//! Pure battle simulation, built on the standalone `bevy_ecs`.
//!
//! Validates the §13 ECS design from the engine blueprint: units are entities,
//! the tick is a chained pipeline of systems, proximity goes through a spatial
//! index (not all-pairs), and the whole thing runs headless for tests.
//!
//! Fase 1a scope: per-group orders (Idle/March/Charge/Hold), per-type movement
//! cooldowns (absolute `next_move_tick`), charge damage bonus, hold damage
//! reduction. Still hand-rolled hex math (hexx lands with A*/terrain in Fase 1c)
//! and a HashMap spatial index (linked-list array version is later).

use bevy_ecs::prelude::*;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Hex (flat-top axial). Hand-rolled for now; `hexx` replaces this in Fase 1c.
// ---------------------------------------------------------------------------

#[derive(Component, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hex {
    pub q: i32,
    pub r: i32,
}

const DIRS: [(i32, i32); 6] = [(1, 0), (1, -1), (0, -1), (-1, 0), (-1, 1), (0, 1)];

impl Hex {
    pub fn new(q: i32, r: i32) -> Self {
        Self { q, r }
    }

    pub fn neighbors(self) -> [Hex; 6] {
        DIRS.map(|(dq, dr)| Hex::new(self.q + dq, self.r + dr))
    }

    /// Axial hex distance.
    pub fn distance(self, o: Hex) -> i32 {
        ((self.q - o.q).abs() + (self.q + self.r - o.q - o.r).abs() + (self.r - o.r).abs()) / 2
    }
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

#[derive(Component, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Team {
    Red,
    Blue,
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Infantry,
    Cavalry,
    Skirmisher,
}

#[derive(Component, Clone, Copy, Debug)]
pub struct Health(pub f32);

/// Group this unit belongs to (1..=4), the unit of command for orders.
#[derive(Component, Clone, Copy, Debug)]
pub struct Group(pub u8);

/// Absolute tick this unit may next move on. Never reset on battle start
/// (the load-bearing invariant from hex-tactics).
#[derive(Component, Clone, Copy, Default, Debug)]
pub struct NextMove(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    Idle,
    March,
    Charge,
    Hold,
}

pub const DAMAGE_PER_TICK: f32 = 14.0;
pub const CHARGE_BONUS: f32 = 10.0;
pub const HOLD_REDUCTION: f32 = 0.5;
pub const VISION: i32 = 8;

pub fn max_hp(kind: Kind) -> f32 {
    match kind {
        Kind::Infantry => 100.0,
        Kind::Cavalry => 80.0,
        Kind::Skirmisher => 60.0,
    }
}

/// Ticks between moves. Cavalry/skirmishers are quicker; charging halves it.
fn move_period(kind: Kind, order: Order) -> u64 {
    let base = match kind {
        Kind::Infantry => 2,
        Kind::Cavalry => 1,
        Kind::Skirmisher => 1,
    };
    if order == Order::Charge {
        (base / 2).max(1)
    } else {
        base
    }
}

/// Component bundle for one unit. Shared by the headless tests and the Bevy
/// game (which adds render components alongside it on the same entity).
pub fn unit(team: Team, kind: Kind, hex: Hex, group: u8) -> impl Bundle {
    (hex, Health(max_hp(kind)), team, kind, Group(group), NextMove(0))
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[derive(Resource, Default)]
pub struct Tick(pub u64);

/// Per-(team, group) orders. Missing entries default to March.
#[derive(Resource, Default)]
pub struct Orders(pub HashMap<(Team, u8), Order>);

impl Orders {
    pub fn get(&self, team: Team, group: u8) -> Order {
        self.0.get(&(team, group)).copied().unwrap_or(Order::March)
    }
    pub fn set(&mut self, team: Team, group: u8, order: Order) {
        self.0.insert((team, group), order);
    }
}

/// One occupant per hex (rigid block). Rebuilt every tick. The cache-friendly
/// linked-list array version (§13) lands when we actually push to thousands.
#[derive(Resource, Default)]
pub struct SpatialIndex {
    pub cells: HashMap<(i32, i32), (Entity, Team)>,
}

impl SpatialIndex {
    fn at(&self, h: Hex) -> Option<(Entity, Team)> {
        self.cells.get(&(h.q, h.r)).copied()
    }
    fn occupied(&self, h: Hex) -> bool {
        self.cells.contains_key(&(h.q, h.r))
    }
}

/// Transient per-tick damage accumulator (scratch), keyed by target entity.
#[derive(Resource, Default)]
pub struct DamageBuffer(pub HashMap<Entity, f32>);

// ---------------------------------------------------------------------------
// Systems — pipeline order mirrors simulateTick's phases.
// ---------------------------------------------------------------------------

pub fn tick_and_clear(mut tick: ResMut<Tick>, mut dmg: ResMut<DamageBuffer>) {
    tick.0 += 1;
    dmg.0.clear();
}

pub fn build_spatial_index(units: Query<(Entity, &Hex, &Team)>, mut idx: ResMut<SpatialIndex>) {
    idx.cells.clear();
    for (e, h, t) in &units {
        idx.cells.insert((h.q, h.r), (e, *t));
    }
}

/// Each unit damages every adjacent enemy; charging attackers hit harder.
pub fn combat(
    units: Query<(&Hex, &Team, &Group)>,
    orders: Res<Orders>,
    idx: Res<SpatialIndex>,
    mut dmg: ResMut<DamageBuffer>,
) {
    for (hex, team, group) in &units {
        let bonus = if orders.get(*team, group.0) == Order::Charge {
            CHARGE_BONUS
        } else {
            0.0
        };
        for n in hex.neighbors() {
            if let Some((enemy, eteam)) = idx.at(n) {
                if eteam != *team {
                    *dmg.0.entry(enemy).or_insert(0.0) += DAMAGE_PER_TICK + bonus;
                }
            }
        }
    }
}

/// Apply accumulated damage (Hold units take less); despawn the dead. Runs
/// before movement so movers never step over a corpse this tick.
pub fn resolve_damage(
    mut commands: Commands,
    mut units: Query<(Entity, &mut Health, &Team, &Group)>,
    orders: Res<Orders>,
    dmg: Res<DamageBuffer>,
) {
    for (e, mut hp, team, group) in &mut units {
        if let Some(d) = dmg.0.get(&e) {
            let mut incoming = *d;
            if orders.get(*team, group.0) == Order::Hold {
                incoming *= 1.0 - HOLD_REDUCTION;
            }
            hp.0 -= incoming;
        }
        if hp.0 <= 0.0 {
            commands.entity(e).despawn();
        }
    }
}

/// Disengaged units step one hex toward the nearest enemy (or the enemy line),
/// gated by their movement cooldown. Hold/Idle stand still.
pub fn movement(
    tick: Res<Tick>,
    orders: Res<Orders>,
    idx: Res<SpatialIndex>,
    mut units: Query<(&mut Hex, &Team, &Kind, &Group, &mut NextMove)>,
) {
    let now = tick.0;
    let mut reserved: HashSet<(i32, i32)> = HashSet::new();

    for (mut hex, team, kind, group, mut next) in &mut units {
        let order = orders.get(*team, group.0);
        if matches!(order, Order::Hold | Order::Idle) {
            continue;
        }
        if now < next.0 {
            continue; // still on cooldown
        }

        let from = *hex;

        // Adjacent to an enemy → engaged, hold position (still fights via combat).
        let fighting = from
            .neighbors()
            .iter()
            .any(|n| matches!(idx.at(*n), Some((_, t)) if t != *team));
        if fighting {
            continue;
        }

        let target = nearest_enemy(&idx, from, *team).unwrap_or(enemy_line(*team));

        // Greedy step: the free neighbor that gets closest to the target.
        let mut best: Option<Hex> = None;
        let mut best_d = i32::MAX;
        for n in from.neighbors() {
            if idx.occupied(n) || reserved.contains(&(n.q, n.r)) {
                continue;
            }
            let d = n.distance(target);
            if d < best_d {
                best_d = d;
                best = Some(n);
            }
        }

        if let Some(n) = best {
            if n.distance(target) < from.distance(target) {
                reserved.insert((n.q, n.r));
                *hex = n;
                next.0 = now + move_period(*kind, order);
            }
        }
    }
}

/// Closest enemy within VISION, found by a range-bounded scan of the index
/// (not an all-pairs sweep) — the property that keeps the tick bounded at scale.
fn nearest_enemy(idx: &SpatialIndex, from: Hex, team: Team) -> Option<Hex> {
    let mut best = None;
    let mut best_d = i32::MAX;
    for dq in -VISION..=VISION {
        for dr in -VISION..=VISION {
            let h = Hex::new(from.q + dq, from.r + dr);
            let d = from.distance(h);
            if d == 0 || d > VISION {
                continue;
            }
            if let Some((_, t)) = idx.at(h) {
                if t != team && d < best_d {
                    best_d = d;
                    best = Some(h);
                }
            }
        }
    }
    best
}

/// Fallback march target: the far side of the field the enemy came from.
fn enemy_line(team: Team) -> Hex {
    match team {
        Team::Red => Hex::new(30, 0),
        Team::Blue => Hex::new(-30, 0),
    }
}

// ---------------------------------------------------------------------------
// Headless driver — used by tests and re-used by the game's tick.
// ---------------------------------------------------------------------------

/// Run one full simulation tick on a bare `World`. No rendering, no app.
pub fn step(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(
        (
            tick_and_clear,
            build_spatial_index,
            combat,
            resolve_damage,
            movement,
        )
            .chain(),
    );
    schedule.run(world);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_world() -> World {
        let mut w = World::new();
        w.insert_resource(Tick::default());
        w.insert_resource(Orders::default());
        w.insert_resource(SpatialIndex::default());
        w.insert_resource(DamageBuffer::default());
        w
    }

    #[test]
    fn adjacent_enemies_deal_damage() {
        let mut w = fresh_world();
        let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));

        step(&mut w);

        let hp = w.get::<Health>(red).expect("red alive").0;
        assert!(hp < max_hp(Kind::Infantry), "red should have taken damage, hp={hp}");
    }

    #[test]
    fn isolated_unit_marches_toward_the_enemy_line() {
        let mut w = fresh_world();
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1)).id();

        step(&mut w);

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert!(h.q < 10, "blue should have advanced toward the enemy line, at {h:?}");
    }

    #[test]
    fn held_units_do_not_move() {
        let mut w = fresh_world();
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::Hold);
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1)).id();

        for _ in 0..10 {
            step(&mut w);
        }

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert_eq!(h, Hex::new(10, 0), "held unit must not move, at {h:?}");
    }

    #[test]
    fn hold_reduces_incoming_damage() {
        // Two identical 1v1s; the Hold defender should end with more HP.
        let dmg_taken = |order: Order| -> f32 {
            let mut w = fresh_world();
            w.resource_mut::<Orders>().set(Team::Red, 1, order);
            let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));
            step(&mut w);
            max_hp(Kind::Infantry) - w.get::<Health>(red).expect("red alive").0
        };
        assert!(
            dmg_taken(Order::Hold) < dmg_taken(Order::March),
            "Hold should reduce incoming damage"
        );
    }

    #[test]
    fn battle_resolves_to_a_decided_outcome() {
        let mut w = fresh_world();
        for r in 0..6 {
            w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, r), 1));
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, r), 1));
        }

        for _ in 0..500 {
            step(&mut w);
        }

        let mut red = 0;
        let mut blue = 0;
        let mut q = w.query::<&Team>();
        for t in q.iter(&w) {
            match t {
                Team::Red => red += 1,
                Team::Blue => blue += 1,
            }
        }
        assert!(red == 0 || blue == 0, "one side should be wiped: red={red} blue={blue}");
    }
}
