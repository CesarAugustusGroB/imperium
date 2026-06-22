//! Pure battle simulation, built on the standalone `bevy_ecs`.
//!
//! Validates the §13 ECS design from the engine blueprint: units are entities,
//! the tick is a chained pipeline of systems, proximity goes through a spatial
//! index (not all-pairs), and the whole thing runs headless for tests.
//!
//! Fase 2c scope: terrain (passability, move cost, defensive bonus) + orders +
//! cooldowns, with movement now routed via **hexx A\*** around obstacles. Hex
//! math stays hand-rolled for the hot per-tick path; hexx is used where it
//! earns its place (pathfinding). The linked-list spatial index is still later.

use bevy_ecs::prelude::*;
use bevy_reflect::Reflect;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Hex (flat-top axial). Hand-rolled math; hexx is used for A* (see movement).
// ---------------------------------------------------------------------------

#[derive(Component, Reflect, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[reflect(Component)]
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
// Terrain
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Terrain {
    Plains,
    Forest,
    Hill,
    Mountain,
    Water,
}

impl Terrain {
    pub fn passable(self) -> bool {
        !matches!(self, Terrain::Mountain | Terrain::Water)
    }
    /// Cooldown multiplier for entering this terrain.
    pub fn move_cost(self) -> u64 {
        match self {
            Terrain::Forest | Terrain::Hill => 2,
            _ => 1,
        }
    }
    /// Multiplier on damage *taken* while standing here (<1 = protective).
    pub fn defense_mult(self) -> f32 {
        match self {
            Terrain::Forest => 0.7,
            Terrain::Hill => 0.6,
            _ => 1.0,
        }
    }
}

#[derive(Resource, Default)]
pub struct TerrainMap {
    pub tiles: HashMap<(i32, i32), Terrain>,
}

impl TerrainMap {
    pub fn get(&self, h: Hex) -> Terrain {
        self.tiles.get(&(h.q, h.r)).copied().unwrap_or(Terrain::Plains)
    }
    pub fn set(&mut self, h: Hex, t: Terrain) {
        self.tiles.insert((h.q, h.r), t);
    }
}

/// Deterministic terrain over an axial rectangle. Coarse 4×4 blobs so biomes
/// clump instead of speckling. Pure hash noise — no dependencies, no RNG state.
pub fn generate_terrain(seed: i32, q_range: i32, r_range: i32) -> TerrainMap {
    let mut map = TerrainMap::default();
    for q in -q_range..=q_range {
        for r in -r_range..=r_range {
            let v = hash01(q.div_euclid(4), r.div_euclid(4), seed);
            let t = if v < 0.10 {
                Terrain::Water
            } else if v < 0.20 {
                Terrain::Mountain
            } else if v < 0.38 {
                Terrain::Forest
            } else if v < 0.52 {
                Terrain::Hill
            } else {
                Terrain::Plains
            };
            map.tiles.insert((q, r), t);
        }
    }
    map
}

fn hash01(a: i32, b: i32, seed: i32) -> f32 {
    let mut h = (a.wrapping_mul(73856093) ^ b.wrapping_mul(19349663) ^ seed.wrapping_mul(83492791))
        as u32;
    h ^= h >> 13;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 16;
    (h as f32) / (u32::MAX as f32)
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

#[derive(Component, Reflect, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[reflect(Component)]
pub enum Team {
    Red,
    Blue,
}

#[derive(Component, Reflect, Clone, Copy, PartialEq, Eq, Debug)]
#[reflect(Component)]
pub enum Kind {
    Infantry,
    Cavalry,
    Skirmisher,
}

#[derive(Component, Reflect, Clone, Copy, Debug)]
#[reflect(Component)]
pub struct Health(pub f32);

/// Group this unit belongs to (1..=4), the unit of command for orders.
#[derive(Component, Reflect, Clone, Copy, Debug)]
#[reflect(Component)]
pub struct Group(pub u8);

/// Absolute tick this unit may next move on. Never reset on battle start
/// (the load-bearing invariant from hex-tactics).
#[derive(Component, Reflect, Clone, Copy, Default, Debug)]
#[reflect(Component)]
pub struct NextMove(pub u64);

/// Animation state of a unit, recomputed every tick from what the unit *did*
/// (see [`animate`]). The render crate keys sprite clips off this — `sim_core`
/// only sets the state, it never renders. `Die` is part of the contract (the
/// catalog has a death clip) but is **never** assigned to a live unit: dead
/// units are despawned, so death is surfaced as a [`DeathEvent`] instead.
#[derive(Component, Reflect, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[reflect(Component)]
pub enum AnimState {
    #[default]
    Idle,
    Move,
    Attack,
    Hit,
    Die,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// Stand still, do nothing offensive beyond defending.
    Idle,
    /// Advance toward the enemy at normal pace.
    March,
    /// Advance fast and hit harder in melee on contact.
    Charge,
    /// Hold position; take reduced damage (dig in).
    Hold,
    /// Disengage and fall back, moving away from the nearest enemy toward
    /// friendly lines. The escape valve for an overwhelmed group.
    Retreat,
    /// All-out commitment: charge pace, the biggest damage bonus, and even
    /// skirmishers drop kiting to close into melee. The "lanzar" of amass→launch.
    Unleash,
}

impl Order {
    /// Orders whose units push toward the enemy (vs. holding or fleeing).
    pub fn is_advancing(self) -> bool {
        matches!(self, Order::March | Order::Charge | Order::Unleash)
    }
}

pub const HOLD_REDUCTION: f32 = 0.5;
pub const VISION: i32 = 8;
/// A skirmisher within this many hexes of an enemy backs off instead of meleeing.
pub const KITE_THRESHOLD: i32 = 2;

/// Ticks an A\* path stays cached before it is recomputed. The README's
/// scalability item: rather than running A\* per unit *per tick*, each unit
/// follows a cached route and only re-plans every few ticks (or when the route
/// is exhausted / the goal drifts). Bounds the per-tick A\* count at scale.
pub const PATH_RECOMPUTE_PERIOD: u64 = 5;
/// If the goal (the nearest enemy) drifts more than this many hexes from the
/// one the cached path was planned for, re-plan early.
const PATH_GOAL_DRIFT: i32 = 2;

/// Per-unit cached A\* route toward its current goal. Pure internal scratch
/// state (not exposed over BRP), so it stays a plain `Component`. `steps` holds
/// the upcoming hexes **reversed** (`last()` = the next step) for O(1) pops.
#[derive(Component, Clone, Default, Debug)]
pub struct PathCache {
    steps: Vec<Hex>,
    goal: Hex,
    stale_at: u64,
}

impl Hex {
    /// `true` once `steps.last()` is no longer one step from `from` — i.e. the
    /// unit drifted off its cached route and the cache must be rebuilt.
    fn off_route(self, next: Option<&Hex>) -> bool {
        next.is_none_or(|n| self.distance(*n) != 1)
    }
}

/// Hexes a unit can strike: melee = 1, skirmishers shoot at range.
pub fn attack_range(kind: Kind) -> i32 {
    match kind {
        Kind::Skirmisher => 3,
        _ => 1,
    }
}

/// Base damage dealt per tick to a target in range.
pub fn attack_damage(kind: Kind) -> f32 {
    match kind {
        Kind::Infantry => 14.0,
        Kind::Cavalry => 11.0,
        Kind::Skirmisher => 8.0,
    }
}

/// Extra damage while committing to the attack. Charge gives the classic bonus
/// (cavalry hits hardest; skirmishers never). Unleash commits everyone and hits
/// harder still — even skirmishers, who are now meleeing rather than kiting.
pub fn charge_bonus(kind: Kind, order: Order) -> f32 {
    match order {
        Order::Charge => match kind {
            Kind::Cavalry => 16.0,
            Kind::Infantry => 8.0,
            Kind::Skirmisher => 0.0,
        },
        Order::Unleash => match kind {
            Kind::Cavalry => 20.0,
            Kind::Infantry => 12.0,
            Kind::Skirmisher => 4.0,
        },
        _ => 0.0,
    }
}

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
    if matches!(order, Order::Charge | Order::Unleash) {
        (base / 2).max(1)
    } else {
        base
    }
}

/// Component bundle for one unit. Shared by the headless tests and the Bevy
/// game (which adds render components alongside it on the same entity).
pub fn unit(team: Team, kind: Kind, hex: Hex, group: u8) -> impl Bundle {
    (
        hex,
        Health(max_hp(kind)),
        team,
        kind,
        Group(group),
        NextMove(0),
        AnimState::default(),
        PathCache::default(),
    )
}

// ---------------------------------------------------------------------------
// Sim events — discrete things the render/audio layer keys animations off.
// Stored in the [`BattleEvents`] log (cleared each tick) rather than Bevy
// `Events<T>` so the headless `step` stays a plain chained schedule with no
// double-buffer update cycle; the render crate can forward them to an
// `EventWriter` if it prefers.
// ---------------------------------------------------------------------------

/// One unit struck another this tick (pre-mitigation). `at` is the attacker's
/// hex — enough for a render layer to spawn a swing/missile effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AttackEvent {
    pub attacker: Entity,
    pub target: Entity,
    pub kind: Kind,
    pub at: Hex,
}

/// A unit died this tick. Carries team/kind/hex because the entity is despawned
/// the same tick, so the render layer cannot look them up afterward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeathEvent {
    pub entity: Entity,
    pub team: Team,
    pub kind: Kind,
    pub at: Hex,
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

/// Per-tick log of sim events the render/audio layer consumes. Cleared at the
/// start of every tick in [`tick_and_clear`]; read after [`step`] returns.
#[derive(Resource, Default)]
pub struct BattleEvents {
    pub attacks: Vec<AttackEvent>,
    pub deaths: Vec<DeathEvent>,
}

/// Scratch: entities that changed hex this tick (drives the `Move` animation
/// state). Cleared each tick. Kept separate from [`BattleEvents`] because
/// movement is continuous state, not a discrete gameplay event.
#[derive(Resource, Default)]
pub struct MovedThisTick(pub HashSet<Entity>);

// ---------------------------------------------------------------------------
// Animation asset catalog — a typed schema the render crate consumes to map a
// unit's (Kind, AnimState) to concrete sprite-sheet frames. Pure data: no Bevy
// asset handles here (that would pull rendering into the headless crate). The
// real art (paths + frame counts) is the human's job; this is the contract.
// ---------------------------------------------------------------------------

/// One animation clip: a contiguous run of frames in a sprite sheet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimClip {
    /// Index of the first frame in the sheet.
    pub first: u32,
    /// Number of frames in the clip.
    pub len: u32,
    /// Playback rate, frames per second.
    pub fps: f32,
    /// `true` for states that hold (idle/move), `false` for one-shots
    /// (attack/hit/die) the render layer plays once.
    pub looping: bool,
}

/// Sprite-sheet (texture atlas) description for one unit kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpriteSheet {
    /// Asset path relative to the render crate's asset root. Placeholder until
    /// art exists.
    pub path: &'static str,
    /// Pixel size of a single frame; frames are uniform.
    pub tile: (u32, u32),
    /// Frames per row in the atlas.
    pub columns: u32,
}

/// A unit kind's full visual: its sheet plus a clip for every [`AnimState`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitVisual {
    pub sheet: SpriteSheet,
    pub idle: AnimClip,
    pub moving: AnimClip,
    pub attack: AnimClip,
    pub hit: AnimClip,
    pub die: AnimClip,
}

impl UnitVisual {
    /// The clip for a given state — the per-frame lookup the render crate makes
    /// from a unit's [`AnimState`].
    pub fn clip(&self, state: AnimState) -> AnimClip {
        match state {
            AnimState::Idle => self.idle,
            AnimState::Move => self.moving,
            AnimState::Attack => self.attack,
            AnimState::Hit => self.hit,
            AnimState::Die => self.die,
        }
    }
}

/// Per-kind visual catalog. A [`Resource`] so the game inserts it once and the
/// render systems read it; absent from the headless `step` pipeline.
#[derive(Resource, Clone, Debug)]
pub struct AnimCatalog {
    pub infantry: UnitVisual,
    pub cavalry: UnitVisual,
    pub skirmisher: UnitVisual,
}

impl AnimCatalog {
    pub fn get(&self, kind: Kind) -> &UnitVisual {
        match kind {
            Kind::Infantry => &self.infantry,
            Kind::Cavalry => &self.cavalry,
            Kind::Skirmisher => &self.skirmisher,
        }
    }
}

impl Default for AnimCatalog {
    /// A placeholder layout: 5 states laid out in rows of an 8-column atlas.
    /// The frame counts/paths are stand-ins so the render crate can wire up
    /// against a real schema before the art lands.
    fn default() -> Self {
        let visual = |path| UnitVisual {
            sheet: SpriteSheet { path, tile: (32, 32), columns: 8 },
            idle: AnimClip { first: 0, len: 8, fps: 6.0, looping: true },
            moving: AnimClip { first: 8, len: 8, fps: 10.0, looping: true },
            attack: AnimClip { first: 16, len: 8, fps: 12.0, looping: false },
            hit: AnimClip { first: 24, len: 4, fps: 12.0, looping: false },
            die: AnimClip { first: 32, len: 6, fps: 8.0, looping: false },
        };
        Self {
            infantry: visual("units/infantry.png"),
            cavalry: visual("units/cavalry.png"),
            skirmisher: visual("units/skirmisher.png"),
        }
    }
}

// ---------------------------------------------------------------------------
// Systems — pipeline order mirrors simulateTick's phases.
// ---------------------------------------------------------------------------

pub fn tick_and_clear(
    mut tick: ResMut<Tick>,
    mut dmg: ResMut<DamageBuffer>,
    mut events: ResMut<BattleEvents>,
    mut moved: ResMut<MovedThisTick>,
) {
    tick.0 += 1;
    dmg.0.clear();
    events.attacks.clear();
    events.deaths.clear();
    moved.0.clear();
}

pub fn build_spatial_index(units: Query<(Entity, &Hex, &Team)>, mut idx: ResMut<SpatialIndex>) {
    idx.cells.clear();
    for (e, h, t) in &units {
        idx.cells.insert((h.q, h.r), (e, *t));
    }
}

/// Melee units damage every adjacent enemy; skirmishers shoot the nearest enemy
/// within missile range. Charging melee attackers hit harder.
pub fn combat(
    units: Query<(Entity, &Hex, &Team, &Kind, &Group)>,
    orders: Res<Orders>,
    idx: Res<SpatialIndex>,
    mut dmg: ResMut<DamageBuffer>,
    mut events: ResMut<BattleEvents>,
) {
    for (me, hex, team, kind, group) in &units {
        let order = orders.get(*team, group.0);
        let amount = attack_damage(*kind) + charge_bonus(*kind, order);
        if attack_range(*kind) == 1 {
            for n in hex.neighbors() {
                if let Some((enemy, eteam)) = idx.at(n) {
                    if eteam != *team {
                        *dmg.0.entry(enemy).or_insert(0.0) += amount;
                        events.attacks.push(AttackEvent {
                            attacker: me,
                            target: enemy,
                            kind: *kind,
                            at: *hex,
                        });
                    }
                }
            }
        } else if let Some(enemy) = nearest_enemy_entity(&idx, *hex, *team, attack_range(*kind)) {
            *dmg.0.entry(enemy).or_insert(0.0) += amount;
            events.attacks.push(AttackEvent {
                attacker: me,
                target: enemy,
                kind: *kind,
                at: *hex,
            });
        }
    }
}

/// Apply accumulated damage; defenders on protective terrain and Hold orders
/// take less. Despawn the dead before movement so movers skip corpses.
pub fn resolve_damage(
    mut commands: Commands,
    mut units: Query<(Entity, &mut Health, &Team, &Kind, &Group, &Hex)>,
    orders: Res<Orders>,
    terrain: Res<TerrainMap>,
    dmg: Res<DamageBuffer>,
    mut events: ResMut<BattleEvents>,
) {
    for (e, mut hp, team, kind, group, hex) in &mut units {
        if let Some(d) = dmg.0.get(&e) {
            let mut incoming = *d * terrain.get(*hex).defense_mult();
            if orders.get(*team, group.0) == Order::Hold {
                incoming *= 1.0 - HOLD_REDUCTION;
            }
            hp.0 -= incoming;
        }
        if hp.0 <= 0.0 {
            events.deaths.push(DeathEvent {
                entity: e,
                team: *team,
                kind: *kind,
                at: *hex,
            });
            commands.entity(e).despawn();
        }
    }
}

/// Disengaged units advance toward the enemy over passable terrain, gated by
/// cooldown (slower through forest/hills). A visible enemy is approached via
/// hexx A* (routes around mountains/water); otherwise a cheap greedy step
/// heads for the enemy line. A* is short-range only (target within VISION) to
/// stay cheap — at thousands of units it must be throttled/cached further.
pub fn movement(
    tick: Res<Tick>,
    orders: Res<Orders>,
    idx: Res<SpatialIndex>,
    terrain: Res<TerrainMap>,
    mut moved: ResMut<MovedThisTick>,
    mut units: Query<(
        Entity,
        &mut Hex,
        &Team,
        &Kind,
        &Group,
        &mut NextMove,
        &mut PathCache,
    )>,
) {
    let now = tick.0;
    let mut reserved: HashSet<(i32, i32)> = HashSet::new();

    for (me, mut hex, team, kind, group, mut next, mut cache) in &mut units {
        let order = orders.get(*team, group.0);
        if matches!(order, Order::Hold | Order::Idle) {
            continue;
        }
        if now < next.0 {
            continue; // still on cooldown
        }

        let from = *hex;
        let enemy = nearest_enemy(&idx, from, *team);

        // Retreat: fall back, away from the nearest enemy, toward friendly lines.
        // Reuses the kiting step (maximise distance from the threat); with no
        // enemy in sight, march toward our own back line instead.
        if order == Order::Retreat {
            let step = match enemy {
                Some(e) => kite_step(from, e, &terrain, &idx, &reserved),
                None => greedy_step(from, own_line(*team), &terrain, &idx, &reserved),
            };
            if let Some(n) = step {
                reserved.insert((n.q, n.r));
                *hex = n;
                moved.0.insert(me);
                next.0 = now + move_period(*kind, order) * terrain.get(n).move_cost();
            }
            continue;
        }

        // Skirmishers kite: back off when an enemy gets close, shooting via
        // combat — unless Unleashed, when they commit to melee like everyone else.
        if *kind == Kind::Skirmisher && order != Order::Unleash {
            if let Some(e) = enemy {
                if from.distance(e) <= KITE_THRESHOLD {
                    if let Some(n) = kite_step(from, e, &terrain, &idx, &reserved) {
                        reserved.insert((n.q, n.r));
                        *hex = n;
                        moved.0.insert(me);
                        next.0 = now + move_period(*kind, order) * terrain.get(n).move_cost();
                    }
                    continue;
                }
            }
        }

        // Melee: adjacent to an enemy → engaged, hold position (still fights).
        let fighting = from
            .neighbors()
            .iter()
            .any(|n| matches!(idx.at(*n), Some((_, t)) if t != *team));
        if fighting {
            continue;
        }

        let goal = enemy.unwrap_or_else(|| enemy_line(*team));

        // Prefer the A*-routed next hex toward a visible enemy; fall back to a
        // greedy step (toward the enemy line, or when the A* hex is taken). The
        // A* route is cached and re-planned only periodically (see PathCache).
        let step = match enemy.and_then(|e| cached_step(from, e, &terrain, now, &mut cache)) {
            Some(s) if !idx.occupied(s) && !reserved.contains(&(s.q, s.r)) => Some(s),
            _ => greedy_step(from, goal, &terrain, &idx, &reserved),
        };

        if let Some(n) = step {
            reserved.insert((n.q, n.r));
            *hex = n;
            moved.0.insert(me);
            next.0 = now + move_period(*kind, order) * terrain.get(n).move_cost();
            // Consume the cached step if we walked onto it; otherwise the next
            // tick sees an off-route cache and re-plans (self-healing).
            if cache.steps.last() == Some(&n) {
                cache.steps.pop();
            }
        }
    }
}

/// Next hex toward `to` along a **cached** A* route, re-planning only when the
/// cache is stale, exhausted, off-route, or the goal has drifted. Recomputing
/// every `PATH_RECOMPUTE_PERIOD` ticks instead of every tick is what keeps the
/// per-tick A* count bounded as unit counts climb into the thousands.
fn cached_step(from: Hex, to: Hex, terrain: &TerrainMap, now: u64, cache: &mut PathCache) -> Option<Hex> {
    let must_replan = now >= cache.stale_at
        || cache.steps.is_empty()
        || from.off_route(cache.steps.last())
        || cache.goal.distance(to) > PATH_GOAL_DRIFT;
    if must_replan {
        cache.steps = a_star_path(from, to, terrain);
        cache.goal = to;
        cache.stale_at = now + PATH_RECOMPUTE_PERIOD;
    }
    cache.steps.last().copied()
}

/// The A* path from `from` to `to` (routing around impassable terrain),
/// excluding `from` and **reversed** so the next step is `last()`. Empty if
/// `to` is unreachable.
fn a_star_path(from: Hex, to: Hex, terrain: &TerrainMap) -> Vec<Hex> {
    let start = hexx::Hex::new(from.q, from.r);
    let end = hexx::Hex::new(to.q, to.r);
    let Some(path) = hexx::algorithms::a_star(start, end, |_, b| {
        let t = terrain.get(Hex::new(b.x, b.y));
        if t.passable() {
            Some(t.move_cost() as u32)
        } else {
            None
        }
    }) else {
        return Vec::new();
    };
    let mut steps: Vec<Hex> = path
        .into_iter()
        .map(|h| Hex::new(h.x, h.y))
        .filter(|h| *h != from)
        .collect();
    steps.reverse();
    steps
}

/// Closest free, passable neighbor that reduces straight-line distance to the
/// goal. Used when there is no visible enemy, or the A* hex is occupied.
fn greedy_step(
    from: Hex,
    goal: Hex,
    terrain: &TerrainMap,
    idx: &SpatialIndex,
    reserved: &HashSet<(i32, i32)>,
) -> Option<Hex> {
    let mut best: Option<Hex> = None;
    let mut best_d = i32::MAX;
    for n in from.neighbors() {
        if !terrain.get(n).passable() || idx.occupied(n) || reserved.contains(&(n.q, n.r)) {
            continue;
        }
        let d = n.distance(goal);
        if d < best_d {
            best_d = d;
            best = Some(n);
        }
    }
    match best {
        Some(n) if n.distance(goal) < from.distance(goal) => Some(n),
        _ => None,
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

/// Entity of the closest enemy within `range` (for ranged attacks).
fn nearest_enemy_entity(idx: &SpatialIndex, from: Hex, team: Team, range: i32) -> Option<Entity> {
    let mut best = None;
    let mut best_d = i32::MAX;
    for dq in -range..=range {
        for dr in -range..=range {
            let h = Hex::new(from.q + dq, from.r + dr);
            let d = from.distance(h);
            if d == 0 || d > range {
                continue;
            }
            if let Some((e, t)) = idx.at(h) {
                if t != team && d < best_d {
                    best_d = d;
                    best = Some(e);
                }
            }
        }
    }
    best
}

/// Free, passable neighbor that moves away from `enemy` (skirmisher kiting).
fn kite_step(
    from: Hex,
    enemy: Hex,
    terrain: &TerrainMap,
    idx: &SpatialIndex,
    reserved: &HashSet<(i32, i32)>,
) -> Option<Hex> {
    let mut best: Option<Hex> = None;
    let mut best_d = -1;
    for n in from.neighbors() {
        if !terrain.get(n).passable() || idx.occupied(n) || reserved.contains(&(n.q, n.r)) {
            continue;
        }
        let d = n.distance(enemy);
        if d > best_d {
            best_d = d;
            best = Some(n);
        }
    }
    match best {
        Some(n) if n.distance(enemy) >= from.distance(enemy) => Some(n),
        _ => None,
    }
}

/// Fallback march target: the far side of the field the enemy came from.
fn enemy_line(team: Team) -> Hex {
    match team {
        Team::Red => Hex::new(30, 0),
        Team::Blue => Hex::new(-30, 0),
    }
}

/// Fallback retreat target: our own back line (opposite the enemy's).
fn own_line(team: Team) -> Hex {
    match team {
        Team::Red => Hex::new(-30, 0),
        Team::Blue => Hex::new(30, 0),
    }
}

// ---------------------------------------------------------------------------
// Enemy AI (Blue). The game opts into this system; the headless `step` pipeline
// stays AI-free so tests drive orders directly.
// ---------------------------------------------------------------------------

/// Army-level order from the current force balance. Emulates the user's style:
/// advance to amass, retreat when crushed, hold when losing, charge when ahead,
/// and unleash an all-out attack when dominant and already in contact.
pub fn ai_order(own: u32, foe: u32, engaged: u32) -> Order {
    if foe == 0 {
        Order::March // nothing to fight → advance
    } else if own * 2 < foe {
        Order::Retreat // routed (< 0.5×) → fall back and regroup
    } else if own * 5 < foe * 4 {
        Order::Hold // outnumbered (< 0.8×) → defend
    } else if engaged > 0 && own * 4 >= foe * 5 {
        Order::Unleash // dominant (≥ 1.25×) and in contact → all-out
    } else if engaged > 0 && own >= foe {
        Order::Charge // in contact and not behind → launch
    } else if engaged > 0 {
        Order::Hold // in contact, roughly even → hold the line
    } else {
        Order::March // not yet in contact → advance and amass
    }
}

/// Sets Blue's orders each tick from the battle state.
pub fn enemy_ai(units: Query<(&Hex, &Team)>, idx: Res<SpatialIndex>, mut orders: ResMut<Orders>) {
    let (mut own, mut foe, mut engaged) = (0u32, 0u32, 0u32);
    for (hex, team) in &units {
        match team {
            Team::Blue => {
                own += 1;
                if hex
                    .neighbors()
                    .iter()
                    .any(|n| matches!(idx.at(*n), Some((_, t)) if t == Team::Red))
                {
                    engaged += 1;
                }
            }
            Team::Red => foe += 1,
        }
    }
    orders.set(Team::Blue, 1, ai_order(own, foe, engaged));
}

// ---------------------------------------------------------------------------
// Animation state machine — runs last, after combat/damage/movement have
// settled, and labels each surviving unit with what it did this tick. Dead
// units are already despawned (their death is a `DeathEvent`).
// ---------------------------------------------------------------------------

/// Recompute every unit's [`AnimState`] from this tick's outcome. Priority,
/// highest first: **Attack** (it dealt damage) > **Hit** (it took damage) >
/// **Move** (it stepped) > **Idle**. Attack outranks Hit so a unit locked in
/// melee — which both deals and takes damage every tick — reads as attacking
/// rather than perpetually flinching.
pub fn animate(
    events: Res<BattleEvents>,
    moved: Res<MovedThisTick>,
    dmg: Res<DamageBuffer>,
    mut units: Query<(Entity, &mut AnimState)>,
) {
    let attackers: HashSet<Entity> = events.attacks.iter().map(|a| a.attacker).collect();
    for (e, mut anim) in &mut units {
        let next = if attackers.contains(&e) {
            AnimState::Attack
        } else if dmg.0.get(&e).is_some_and(|d| *d > 0.0) {
            AnimState::Hit
        } else if moved.0.contains(&e) {
            AnimState::Move
        } else {
            AnimState::Idle
        };
        if *anim != next {
            *anim = next;
        }
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
            animate,
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
        w.insert_resource(TerrainMap::default());
        w.insert_resource(SpatialIndex::default());
        w.insert_resource(DamageBuffer::default());
        w.insert_resource(BattleEvents::default());
        w.insert_resource(MovedThisTick::default());
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
    fn units_do_not_step_onto_impassable_terrain() {
        let mut w = fresh_world();
        // Blue at (10,0) wants to march toward -q. Wall the two left-ward
        // neighbors with mountains; it must not enter them.
        {
            let mut t = w.resource_mut::<TerrainMap>();
            t.set(Hex::new(9, 0), Terrain::Mountain);
            t.set(Hex::new(9, 1), Terrain::Mountain);
        }
        w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1));

        step(&mut w);

        let occupied: Vec<Hex> = {
            let mut q = w.query::<&Hex>();
            q.iter(&w).copied().collect()
        };
        assert!(
            !occupied.contains(&Hex::new(9, 0)) && !occupied.contains(&Hex::new(9, 1)),
            "unit must not step onto a mountain: {occupied:?}"
        );
    }

    #[test]
    fn a_star_routes_around_a_wall() {
        // Blue at (0,0); enemy at (3,0). A mountain wall at (1,0) and (1,1)
        // blocks the straight line, leaving a gap at (1,-1). The unit must step
        // around the wall, never onto a mountain.
        let mut w = fresh_world();
        {
            let mut t = w.resource_mut::<TerrainMap>();
            t.set(Hex::new(1, 0), Terrain::Mountain);
            t.set(Hex::new(1, 1), Terrain::Mountain);
        }
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(0, 0), 1)).id();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(3, 0), 1));

        step(&mut w);

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert_ne!(h, Hex::new(0, 0), "unit should have moved");
        assert!(
            h != Hex::new(1, 0) && h != Hex::new(1, 1),
            "unit must route around the wall, not onto it: {h:?}"
        );
    }

    #[test]
    fn defensive_terrain_reduces_damage() {
        let dmg_on = |terrain: Terrain| -> f32 {
            let mut w = fresh_world();
            w.resource_mut::<TerrainMap>().set(Hex::new(0, 0), terrain);
            let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));
            step(&mut w);
            max_hp(Kind::Infantry) - w.get::<Health>(red).expect("red alive").0
        };
        assert!(
            dmg_on(Terrain::Hill) < dmg_on(Terrain::Plains),
            "defending on a hill should reduce damage taken"
        );
    }

    #[test]
    fn skirmishers_shoot_at_range_melee_does_not() {
        let dmg_to_enemy = |kind: Kind| -> f32 {
            let mut w = fresh_world();
            w.spawn(unit(Team::Red, kind, Hex::new(0, 0), 1));
            let blue = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(3, 0), 1)).id();
            step(&mut w);
            max_hp(Kind::Infantry) - w.get::<Health>(blue).map(|h| h.0).unwrap_or(0.0)
        };
        assert!(dmg_to_enemy(Kind::Skirmisher) > 0.0, "skirmisher should hit at range 3");
        assert_eq!(dmg_to_enemy(Kind::Infantry), 0.0, "melee must not hit at range 3");
    }

    #[test]
    fn skirmishers_kite_from_adjacent_enemies() {
        let mut w = fresh_world();
        let sk = w.spawn(unit(Team::Red, Kind::Skirmisher, Hex::new(0, 0), 1)).id();
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));

        step(&mut w);

        let h = *w.get::<Hex>(sk).expect("skirmisher alive");
        assert!(h.distance(Hex::new(1, 0)) >= 2, "skirmisher should kite away, at {h:?}");
    }

    #[test]
    fn ai_advances_then_commits_and_defends() {
        assert_eq!(ai_order(10, 0, 0), Order::March, "no enemy → advance");
        assert_eq!(ai_order(10, 10, 0), Order::March, "even, no contact → advance");
        assert_eq!(ai_order(10, 10, 3), Order::Charge, "contact + not behind → launch");
        assert_eq!(ai_order(8, 10, 3), Order::Hold, "contact, slightly behind → hold");
        assert_eq!(ai_order(5, 10, 3), Order::Hold, "outnumbered → defend");
        assert_eq!(ai_order(20, 10, 3), Order::Unleash, "dominant + contact → all-out");
        assert_eq!(ai_order(3, 10, 0), Order::Retreat, "routed → fall back");
    }

    #[test]
    fn unleash_hits_harder_and_moves_faster_than_charge() {
        for kind in [Kind::Infantry, Kind::Cavalry] {
            assert!(
                charge_bonus(kind, Order::Unleash) > charge_bonus(kind, Order::Charge),
                "{kind:?}: unleash should out-hit charge"
            );
        }
        // Skirmishers get no charge bonus but do commit under Unleash.
        assert_eq!(charge_bonus(Kind::Skirmisher, Order::Charge), 0.0);
        assert!(charge_bonus(Kind::Skirmisher, Order::Unleash) > 0.0);
        // Unleash runs at charge pace (faster than a plain march).
        assert!(
            move_period(Kind::Infantry, Order::Unleash) < move_period(Kind::Infantry, Order::March),
            "unleash should move faster than march"
        );
    }

    #[test]
    fn retreating_units_flee_from_the_enemy() {
        let mut w = fresh_world();
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::Retreat);
        let blue = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, 0), 1)).id();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(3, 0), 1));

        let before = Hex::new(0, 0).distance(Hex::new(3, 0));
        step(&mut w);

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert!(
            h.distance(Hex::new(3, 0)) > before,
            "retreating unit should increase distance from the enemy, at {h:?}"
        );
    }

    #[test]
    fn retreat_with_no_enemy_falls_back_to_own_line() {
        // Blue's own line is +q; a lone retreating Blue with no enemy in sight
        // should drift toward it rather than freeze.
        let mut w = fresh_world();
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::Retreat);
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(0, 0), 1)).id();

        step(&mut w);

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert!(h.q > 0, "retreating unit should fall back toward its own line, at {h:?}");
    }

    #[test]
    fn unleashed_skirmishers_close_instead_of_kiting() {
        // A kiting skirmisher backs away from an adjacent enemy; an Unleashed one
        // does not — it commits to melee and stays in contact.
        let final_distance = |order: Order| -> i32 {
            let mut w = fresh_world();
            w.resource_mut::<Orders>().set(Team::Red, 1, order);
            let sk = w.spawn(unit(Team::Red, Kind::Skirmisher, Hex::new(0, 0), 1)).id();
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));
            step(&mut w);
            w.get::<Hex>(sk).expect("skirmisher alive").distance(Hex::new(1, 0))
        };
        assert!(final_distance(Order::March) >= 2, "kiting skirmisher backs off");
        assert_eq!(
            final_distance(Order::Unleash),
            1,
            "unleashed skirmisher holds contact instead of kiting"
        );
    }

    #[test]
    fn cached_pathing_routes_around_a_wall_across_many_ticks() {
        // A mountain wall blocks the straight line from (0,0) to a stationary
        // enemy 8 hexes away. The mover threads the one-hex gap and closes in
        // over several ticks — spanning multiple path-cache recomputes — and
        // must never stand on a mountain, proving the cached route stays correct
        // as it is consumed and re-planned. We stop the moment it reaches
        // contact (before melee can resolve) so the assertion is about pathing.
        let enemy = Hex::new(8, 0);
        let mut w = fresh_world();
        {
            let mut t = w.resource_mut::<TerrainMap>();
            for r in -1..=2 {
                t.set(Hex::new(2, r), Terrain::Mountain); // gap left at (2,-2)
            }
        }
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(0, 0), 1)).id();
        // Stationary enemy (Hold) so the goal never drifts.
        w.spawn(unit(Team::Red, Kind::Infantry, enemy, 1));
        w.resource_mut::<Orders>().set(Team::Red, 1, Order::Hold);
        let start_d = Hex::new(0, 0).distance(enemy);

        let mut ticks = 0;
        let mut reached = false;
        for _ in 0..30 {
            step(&mut w);
            ticks += 1;
            let h = *w.get::<Hex>(blue).expect("blue alive");
            assert!(
                w.resource::<TerrainMap>().get(h).passable(),
                "mover stood on impassable terrain at {h:?}"
            );
            if h.distance(enemy) <= 1 {
                reached = true;
                break;
            }
        }

        let end = *w.get::<Hex>(blue).expect("blue alive");
        assert!(reached, "mover should have reached contact, ended at {end:?}");
        assert!(
            ticks as u64 > PATH_RECOMPUTE_PERIOD,
            "approach should span multiple cache recomputes, took {ticks} ticks"
        );
        assert!(end.distance(enemy) < start_d, "mover should have closed in");
    }

    #[test]
    fn stress_thousands_of_units_resolve_without_panic() {
        // Headless scale smoke test: two large blocks collide and run for a
        // good many ticks. Asserts the tick stays panic-free and the live unit
        // count is monotonically non-increasing (units only ever die). This is
        // the harness for the throttled-A* / spatial-index scalability work.
        let mut w = fresh_world();
        let per_side = 768;
        let cols = 16;
        for i in 0..per_side {
            let (q, r) = (i % cols, i / cols);
            w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(-1 - q, r), 1));
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1 + q, r), 1));
        }

        let mut prev = count_units(&mut w);
        assert_eq!(prev, per_side as usize * 2, "all units spawned");
        for _ in 0..60 {
            step(&mut w);
            let now = count_units(&mut w);
            assert!(now <= prev, "unit count must never grow: {now} > {prev}");
            prev = now;
        }
        assert!(prev < per_side as usize * 2, "some units should have died");
    }

    fn count_units(w: &mut World) -> usize {
        w.query::<&Team>().iter(w).count()
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

    // --- Animation state machine + events -------------------------------

    #[test]
    fn attacking_units_enter_the_attack_state_and_log_an_event() {
        let mut w = fresh_world();
        let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
        let blue = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1)).id();

        step(&mut w);

        // Both are locked in melee: each deals damage, so each reads as Attack
        // (Attack outranks Hit).
        assert_eq!(*w.get::<AnimState>(red).unwrap(), AnimState::Attack);
        assert_eq!(*w.get::<AnimState>(blue).unwrap(), AnimState::Attack);

        let ev = w.resource::<BattleEvents>();
        assert!(
            ev.attacks.contains(&AttackEvent {
                attacker: red,
                target: blue,
                kind: Kind::Infantry,
                at: Hex::new(0, 0),
            }),
            "red→blue strike should be logged: {:?}",
            ev.attacks
        );
    }

    #[test]
    fn marching_units_enter_the_move_state() {
        let mut w = fresh_world();
        // Lone cavalry, no enemy: it marches toward the enemy line.
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1)).id();

        step(&mut w);

        assert_eq!(*w.get::<AnimState>(blue).unwrap(), AnimState::Move);
    }

    #[test]
    fn a_struck_survivor_shows_hit_not_move() {
        let mut w = fresh_world();
        // Infantry marches at a skirmisher out of its melee reach: it advances
        // (would be Move) *and* gets shot. Hit must win over Move.
        let inf = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
        let sk = w.spawn(unit(Team::Blue, Kind::Skirmisher, Hex::new(3, 0), 1)).id();

        step(&mut w);

        assert_eq!(*w.get::<AnimState>(inf).unwrap(), AnimState::Hit, "shot melee = Hit");
        assert_eq!(*w.get::<AnimState>(sk).unwrap(), AnimState::Attack, "shooter = Attack");
    }

    #[test]
    fn idle_units_settle_into_the_idle_state() {
        let mut w = fresh_world();
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1)).id();

        // First tick it marches (Move) — prove the system actively transitions.
        step(&mut w);
        assert_eq!(*w.get::<AnimState>(blue).unwrap(), AnimState::Move);

        // Hold it: no movement, no combat → back to Idle.
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::Hold);
        step(&mut w);
        assert_eq!(*w.get::<AnimState>(blue).unwrap(), AnimState::Idle);
    }

    #[test]
    fn death_emits_an_event_with_team_kind_and_position() {
        let mut w = fresh_world();
        let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));
        // One blue infantry strike is 14 dmg; drop red to a lethal sliver.
        w.get_mut::<Health>(red).unwrap().0 = 5.0;

        step(&mut w);

        assert!(w.get::<Health>(red).is_none(), "red should be despawned");
        let deaths = &w.resource::<BattleEvents>().deaths;
        assert_eq!(
            deaths,
            &vec![DeathEvent {
                entity: red,
                team: Team::Red,
                kind: Kind::Infantry,
                at: Hex::new(0, 0),
            }],
            "death event must carry team/kind/position"
        );
    }

    #[test]
    fn per_tick_buffers_reset_between_ticks() {
        let mut w = fresh_world();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1));
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1));

        step(&mut w);
        assert!(!w.resource::<BattleEvents>().attacks.is_empty(), "tick 1 logged attacks");

        // Pull them apart so nothing happens, then tick: the log must clear.
        let reds: Vec<Entity> = {
            let mut q = w.query_filtered::<Entity, With<Team>>();
            q.iter(&w).collect()
        };
        for e in reds {
            if w.get::<Team>(e) == Some(&Team::Blue) {
                w.get_mut::<Hex>(e).unwrap().q = 40; // far away
            }
        }
        w.resource_mut::<Orders>().set(Team::Red, 1, Order::Hold);
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::Hold);

        step(&mut w);
        let ev = w.resource::<BattleEvents>();
        assert!(ev.attacks.is_empty() && ev.deaths.is_empty(), "buffers must reset each tick");
        assert!(w.resource::<MovedThisTick>().0.is_empty(), "moved set must reset each tick");
    }

    // --- Animation asset catalog ----------------------------------------

    #[test]
    fn catalog_maps_each_kind_and_state_to_a_clip() {
        let cat = AnimCatalog::default();
        for kind in [Kind::Infantry, Kind::Cavalry, Kind::Skirmisher] {
            let v = cat.get(kind);
            // Every state resolves and the clip lookup matches the field.
            assert_eq!(v.clip(AnimState::Idle), v.idle);
            assert_eq!(v.clip(AnimState::Move), v.moving);
            assert_eq!(v.clip(AnimState::Attack), v.attack);
            assert_eq!(v.clip(AnimState::Hit), v.hit);
            assert_eq!(v.clip(AnimState::Die), v.die);
        }
    }

    #[test]
    fn default_catalog_loops_holds_but_not_one_shots() {
        let v = AnimCatalog::default().get(Kind::Infantry).clone();
        assert!(v.idle.looping && v.moving.looping, "idle/move should loop");
        assert!(
            !v.attack.looping && !v.hit.looping && !v.die.looping,
            "attack/hit/die should play once"
        );
        // Each kind points at a distinct sheet path.
        let cat = AnimCatalog::default();
        let paths = [
            cat.get(Kind::Infantry).sheet.path,
            cat.get(Kind::Cavalry).sheet.path,
            cat.get(Kind::Skirmisher).sheet.path,
        ];
        assert_eq!(
            paths.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "kinds should map to distinct sheets: {paths:?}"
        );
    }
}
