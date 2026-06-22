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

    /// The hexes the straight line from `self` to `o` passes through, endpoints
    /// included, ordered from `self` to `o`. Uses cube-coordinate linear
    /// interpolation with hex rounding (Red Blob Games). Deterministic.
    pub fn line_to(self, o: Hex) -> Vec<Hex> {
        let n = self.distance(o);
        if n == 0 {
            return vec![self];
        }
        // Cube coords: x=q, z=r, y=-x-z. Nudge endpoints by a tiny epsilon so a
        // line that grazes a hex edge resolves consistently instead of flapping.
        let (ax, ay, az) = (self.q as f32, (-self.q - self.r) as f32, self.r as f32);
        let (bx, by, bz) = (o.q as f32, (-o.q - o.r) as f32, o.r as f32);
        (0..=n)
            .map(|i| {
                let t = i as f32 / n as f32;
                cube_round(
                    ax + (bx - ax) * t,
                    ay + (by - ay) * t,
                    az + (bz - az) * t,
                )
            })
            .collect()
    }
}

/// Round fractional cube coords to the nearest hex, preserving x+y+z==0. We
/// return only q (=x) and r (=z); the coordinate with the largest rounding
/// error is recomputed from the other two so the triple stays consistent.
fn cube_round(xf: f32, yf: f32, zf: f32) -> Hex {
    let (mut x, y, mut z) = (xf.round(), yf.round(), zf.round());
    let (dx, dy, dz) = ((x - xf).abs(), (y - yf).abs(), (z - zf).abs());
    if dx > dy && dx > dz {
        x = -y - z; // x had the worst error → derive it from y, z
    } else if dz > dy {
        z = -x - y; // z had the worst error → derive it from x, y
    }
    // Otherwise y was worst; we discard it, so x and z need no correction.
    Hex::new(x as i32, z as i32)
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
    /// Whether this terrain blocks ranged line of sight when it lies *between*
    /// a shooter and its target. Mountains are tall enough that missiles cannot
    /// pass through them; everything else (including water, which arrows fly
    /// over) is clear. Only intervening hexes block — see `line_of_sight`.
    pub fn blocks_sight(self) -> bool {
        matches!(self, Terrain::Mountain)
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

/// One occupant per hex (rigid block), stored in a **dense array** over the
/// bounding box of all occupied hexes — contiguous memory, hash-free O(1)
/// lookups, rebuilt every tick. Replaces the per-hex `HashMap` placeholder
/// (§13): at thousands of units the array's cache locality and the absence of
/// per-probe hashing dominate the (linear) rebuild cost.
///
/// The grid spans exactly the occupied bounding box, so it stays compact for an
/// army clustered on a battlefield. (A unit that strays far from the pack would
/// inflate the box; the realistic, converging armies this engine simulates do
/// not.) Lookups outside the box always miss.
#[derive(Resource, Default)]
pub struct SpatialIndex {
    q_min: i32,
    r_min: i32,
    width: i32,
    height: i32,
    cells: Vec<Option<(Entity, Team)>>,
}

impl SpatialIndex {
    /// Linear cell index for `h`, or `None` if it lies outside the current
    /// bounding box (and therefore cannot be occupied).
    fn index(&self, h: Hex) -> Option<usize> {
        let x = h.q - self.q_min;
        let y = h.r - self.r_min;
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        Some((y * self.width + x) as usize)
    }

    fn at(&self, h: Hex) -> Option<(Entity, Team)> {
        self.index(h).and_then(|i| self.cells[i])
    }

    fn occupied(&self, h: Hex) -> bool {
        self.at(h).is_some()
    }

    /// Resize the dense grid to `bounds` (`(q_min, r_min, q_max, r_max)`) and
    /// clear it. `None` (an empty world) yields an empty grid that misses every
    /// lookup. Reuses the existing allocation across ticks.
    fn reset(&mut self, bounds: Option<(i32, i32, i32, i32)>) {
        match bounds {
            None => {
                self.q_min = 0;
                self.r_min = 0;
                self.width = 0;
                self.height = 0;
                self.cells.clear();
            }
            Some((q_min, r_min, q_max, r_max)) => {
                self.q_min = q_min;
                self.r_min = r_min;
                self.width = q_max - q_min + 1;
                self.height = r_max - r_min + 1;
                let len = (self.width as usize) * (self.height as usize);
                self.cells.clear();
                self.cells.resize(len, None);
            }
        }
    }

    /// Mark `h` occupied. Last write wins (matching the old `HashMap` insert);
    /// out-of-bounds writes are impossible since `reset` sized the box to fit.
    fn set(&mut self, h: Hex, e: Entity, t: Team) {
        if let Some(i) = self.index(h) {
            self.cells[i] = Some((e, t));
        }
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

/// Shared-goal navigation field. For each team, a Dijkstra **integration field**
/// over the terrain: `dist[(q, r)]` is the cheapest move-cost to reach that
/// team's `enemy_line` from `(q, r)`, routing around impassable terrain. A unit
/// with no enemy in sight follows the descending gradient instead of a private
/// greedy step — so it flows around concave obstacles a greedy step dead-ends
/// in, and the whole army shares **one** computed field (no per-unit A\*).
///
/// The field depends only on the (static) terrain and the fixed goal, so it is
/// built once and reused for the rest of the battle.
#[derive(Resource, Default)]
pub struct FlowField {
    dist: HashMap<Team, HashMap<(i32, i32), u32>>,
    built: bool,
}

/// How far ahead of its group's front a unit may get before it pauses to let
/// the formation close up (in hexes of advance toward the enemy line).
pub const COHESION_SLACK: f32 = 4.0;

/// Per-(team, group) formation state: the mean advance front, i.e. the average
/// distance of the group's living members to their enemy line. Rebuilt each
/// tick so cohesion tracks the formation as it moves and takes casualties.
#[derive(Resource, Default)]
pub struct Formations {
    pub mean_dist: HashMap<(Team, u8), f32>,
}

impl Formations {
    /// Mean distance-to-enemy-line for a group, or `None` if it has no members.
    fn front(&self, team: Team, group: u8) -> Option<f32> {
        self.mean_dist.get(&(team, group)).copied()
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
    // Pass 1: bounding box of every occupied hex (so the grid stays compact).
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    for (_, h, _) in &units {
        bounds = Some(match bounds {
            None => (h.q, h.r, h.q, h.r),
            Some((qn, rn, qx, rx)) => (qn.min(h.q), rn.min(h.r), qx.max(h.q), rx.max(h.r)),
        });
    }

    // Pass 2: size the dense grid to that box and fill it.
    idx.reset(bounds);
    for (e, h, t) in &units {
        idx.set(*h, e, *t);
    }
}

/// Build each team's flow field once, lazily, from the static terrain. Cheap
/// no-op on every later tick (and while terrain is still empty). Runs before
/// movement; only reads terrain, so it never races the per-tick state.
pub fn build_flow_fields(terrain: Res<TerrainMap>, mut field: ResMut<FlowField>) {
    if field.built || terrain.tiles.is_empty() {
        return;
    }
    field.built = true;
    for team in [Team::Red, Team::Blue] {
        field.dist.insert(team, integrate(&terrain, enemy_line(team)));
    }
}

/// Dijkstra from `goal` outward over passable terrain, yielding cost-to-goal per
/// cell. Expansion is bounded to cells present in the terrain map (the
/// battlefield), so the field stays finite even though the hex plane is not.
/// Entering a cell costs that cell's `move_cost`, mirroring the A\* cost model.
fn integrate(terrain: &TerrainMap, goal: Hex) -> HashMap<(i32, i32), u32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let mut dist: HashMap<(i32, i32), u32> = HashMap::new();
    if !terrain.get(goal).passable() {
        return dist; // goal itself is in a wall → unreachable, empty field
    }
    dist.insert((goal.q, goal.r), 0);
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((0u32, goal.q, goal.r)));
    while let Some(Reverse((d, q, r))) = heap.pop() {
        if d > *dist.get(&(q, r)).unwrap_or(&u32::MAX) {
            continue; // stale heap entry
        }
        for n in Hex::new(q, r).neighbors() {
            // Only flow into cells the map actually covers (bounds the field).
            if !terrain.tiles.contains_key(&(n.q, n.r)) {
                continue;
            }
            let t = terrain.get(n);
            if !t.passable() {
                continue;
            }
            let nd = d + t.move_cost() as u32;
            if nd < *dist.get(&(n.q, n.r)).unwrap_or(&u32::MAX) {
                dist.insert((n.q, n.r), nd);
                heap.push(Reverse((nd, n.q, n.r)));
            }
        }
    }
    dist
}

/// Compute each group's average distance to its enemy line — the formation's
/// advance "front". `movement` reads this so a unit that has outrun its group
/// can pause and let the block catch up. O(units), no proximity queries.
pub fn build_formations(units: Query<(&Hex, &Team, &Group)>, mut formations: ResMut<Formations>) {
    let mut sum: HashMap<(Team, u8), (i64, u32)> = HashMap::new();
    for (hex, team, group) in &units {
        let d = hex.distance(enemy_line(*team)) as i64;
        let acc = sum.entry((*team, group.0)).or_insert((0, 0));
        acc.0 += d;
        acc.1 += 1;
    }
    formations.mean_dist.clear();
    for (key, (total, count)) in sum {
        formations.mean_dist.insert(key, total as f32 / count as f32);
    }
}

/// Each unit lands ONE attack per tick. Melee attackers concentrate on a single
/// adjacent enemy — the lowest-HP one, to secure kills (focus fire) — rather than
/// striking every neighbor at once; skirmishers shoot the nearest enemy in
/// missile range. Charging melee attackers hit harder.
///
/// Single-targeting is what makes flanking matter: a surrounded unit is struck by
/// every neighbor but only strikes back at one, so being outnumbered in melee is
/// lethal. (Previously a unit dealt its full damage to all six neighbors at once,
/// which perversely rewarded being surrounded.)
pub fn combat(
    units: Query<(Entity, &Hex, &Team, &Kind, &Group)>,
    healths: Query<&Health>,
    orders: Res<Orders>,
    idx: Res<SpatialIndex>,
    terrain: Res<TerrainMap>,
    mut dmg: ResMut<DamageBuffer>,
    mut events: ResMut<BattleEvents>,
) {
    for (me, hex, team, kind, group) in &units {
        let order = orders.get(*team, group.0);
        let amount = attack_damage(*kind) + charge_bonus(*kind, order);
        let target = if attack_range(*kind) == 1 {
            weakest_adjacent_enemy(hex, *team, &idx, &healths)
        } else {
            nearest_enemy_entity(&idx, *hex, *team, attack_range(*kind), &terrain)
        };
        if let Some(enemy) = target {
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

/// Lowest-HP enemy in one of `hex`'s six neighbors, or `None`. Ties break by the
/// fixed neighbor order, so the choice is deterministic.
fn weakest_adjacent_enemy(
    hex: &Hex,
    team: Team,
    idx: &SpatialIndex,
    healths: &Query<&Health>,
) -> Option<Entity> {
    let mut target: Option<Entity> = None;
    let mut best_hp = f32::INFINITY;
    for n in hex.neighbors() {
        if let Some((enemy, eteam)) = idx.at(n) {
            if eteam != team {
                let hp = healths.get(enemy).map(|h| h.0).unwrap_or(f32::INFINITY);
                if hp < best_hp {
                    best_hp = hp;
                    target = Some(enemy);
                }
            }
        }
    }
    target
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
    field: Res<FlowField>,
    formations: Res<Formations>,
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

        // Formation cohesion: on the open advance (no enemy in sight, plain
        // March) a unit that has pushed more than COHESION_SLACK ahead of its
        // group's front pauses, letting the block close up instead of smearing
        // into a thin line. Committed orders (Charge/Unleash) and any unit that
        // can see an enemy ignore cohesion — contact always overrides ranks.
        if enemy.is_none() && order == Order::March {
            if let Some(front) = formations.front(*team, group.0) {
                let my_dist = from.distance(enemy_line(*team)) as f32;
                if my_dist + COHESION_SLACK < front {
                    continue; // ahead of the pack — hold for the formation
                }
            }
        }

        // With an enemy in sight, route to it along a **cached** A* path
        // (re-planned only periodically, see PathCache); fall back to a greedy
        // step, then a sidestep when boxed in by packed ranks. With no enemy in
        // sight, the goal is the shared enemy line — follow the flow field (one
        // field for the whole army; routes around concave terrain a greedy step
        // dead-ends in), falling back to greedy/sidestep on open ground.
        let step = if let Some(e) = enemy {
            match cached_step(from, e, &terrain, now, &mut cache) {
                Some(s) if !idx.occupied(s) && !reserved.contains(&(s.q, s.r)) => Some(s),
                _ => greedy_step(from, e, &terrain, &idx, &reserved)
                    .or_else(|| sidestep(from, e, &terrain, &idx, &reserved)),
            }
        } else {
            let line = enemy_line(*team);
            flow_step(&field, *team, from, &terrain, &idx, &reserved)
                .or_else(|| greedy_step(from, line, &terrain, &idx, &reserved))
                .or_else(|| sidestep(from, line, &terrain, &idx, &reserved))
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

/// Anti-gridlock valve: when no neighbor makes progress toward `goal` (every
/// distance-reducing hex is taken), slip to a free neighbor that at least *holds*
/// the distance, letting a stalled unit flow laterally around the jam ahead. It
/// never steps backward (a strictly-farther hex is no better than waiting), so
/// units don't scatter — they only peel sideways when genuinely boxed in.
fn sidestep(
    from: Hex,
    goal: Hex,
    terrain: &TerrainMap,
    idx: &SpatialIndex,
    reserved: &HashSet<(i32, i32)>,
) -> Option<Hex> {
    let cur = from.distance(goal);
    let mut best: Option<Hex> = None;
    let mut best_d = i32::MAX;
    for n in from.neighbors() {
        if !terrain.get(n).passable() || idx.occupied(n) || reserved.contains(&(n.q, n.r)) {
            continue;
        }
        let d = n.distance(goal);
        if d <= cur && d < best_d {
            best_d = d;
            best = Some(n);
        }
    }
    best
}

/// Next hex down the flow field's gradient toward the team's enemy line: the
/// free, passable neighbor with the lowest cost-to-goal. A unit already on the
/// field only steps to a *strictly* closer cell (no oscillation on a plateau);
/// a unit not yet on the field steps onto the nearest field cell to join it.
/// `None` when there is no field for the team or no usable neighbor — the caller
/// then falls back to the greedy step.
fn flow_step(
    field: &FlowField,
    team: Team,
    from: Hex,
    terrain: &TerrainMap,
    idx: &SpatialIndex,
    reserved: &HashSet<(i32, i32)>,
) -> Option<Hex> {
    let dist = field.dist.get(&team)?;
    let mut best: Option<Hex> = None;
    let mut best_nd = u32::MAX;
    for n in from.neighbors() {
        if !terrain.get(n).passable() || idx.occupied(n) || reserved.contains(&(n.q, n.r)) {
            continue;
        }
        if let Some(&nd) = dist.get(&(n.q, n.r)) {
            if nd < best_nd {
                best_nd = nd;
                best = Some(n);
            }
        }
    }
    let best = best?;
    match dist.get(&(from.q, from.r)) {
        Some(&here) if best_nd < here => Some(best), // descend the gradient
        Some(_) => None,                             // plateau/uphill → let greedy decide
        None => Some(best),                          // off-field → step on to join it
    }
}

/// Whether nothing sight-blocking (a mountain) sits strictly between `from` and
/// `to`. The endpoints never block: a target standing on a mountain is still
/// shootable, and a shooter on one can still fire out.
pub fn line_of_sight(from: Hex, to: Hex, terrain: &TerrainMap) -> bool {
    let line = from.line_to(to);
    // Skip the first and last hexes (the endpoints).
    line[1..line.len().saturating_sub(1)]
        .iter()
        .all(|h| !terrain.get(*h).blocks_sight())
}

/// Entity of the closest enemy within `range` that the shooter has line of
/// sight to (mountains block missiles). Returns the nearest *unobstructed* foe,
/// so a skirmisher whose closest enemy hides behind a mountain still shoots a
/// farther one it can actually see.
fn nearest_enemy_entity(
    idx: &SpatialIndex,
    from: Hex,
    team: Team,
    range: i32,
    terrain: &TerrainMap,
) -> Option<Entity> {
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
                if t != team && d < best_d && line_of_sight(from, h, terrain) {
                    best_d = d;
                    best = Some(e);
                }
            }
        }
    }
    best
}

/// Nearest enemy within `range`, scanning the hex disk **ring by ring outward**
/// and stopping at the first ring that holds one — so a unit with an adjacent
/// foe touches ~6 cells instead of the whole `(2·range+1)²` bounding box. This
/// is the property that keeps the proximity probe cheap at scale: the common
/// case (an enemy nearby) costs O(closest ring), and even the empty case walks
/// the disk (≈3·range²) rather than the larger square. Every cell of a ring is
/// equidistant from `from`, so the first hit in a ring is a true nearest; ties
/// resolve by the fixed ring-walk order, keeping the scan deterministic.
fn nearest_enemy_in_range(
    idx: &SpatialIndex,
    from: Hex,
    team: Team,
    range: i32,
) -> Option<(Entity, Hex)> {
    for radius in 1..=range {
        // Walk the ring of `radius` cells around `from` (red-blob algorithm:
        // start at the corner `DIRS[4]·radius` away, then trace each of the six
        // sides). No allocation — the hot path runs this per unit per tick.
        let (sq, sr) = DIRS[4];
        let mut h = Hex::new(from.q + sq * radius, from.r + sr * radius);
        for (dq, dr) in DIRS {
            for _ in 0..radius {
                if let Some((e, t)) = idx.at(h) {
                    if t != team {
                        return Some((e, h));
                    }
                }
                h = Hex::new(h.q + dq, h.r + dr);
            }
        }
    }
    None
}

/// Closest enemy hex within VISION (movement target).
fn nearest_enemy(idx: &SpatialIndex, from: Hex, team: Team) -> Option<Hex> {
    nearest_enemy_in_range(idx, from, team, VISION).map(|(_, h)| h)
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
    // Widen to u64: `own * 5` overflows a u32 past ~858M units, and the project
    // is explicitly aiming for tens of thousands → millions. Overflow is a
    // debug-build panic (and silent wraparound in release), so the balance
    // math must not lose precision at scale.
    let (own, foe) = (u64::from(own), u64::from(foe));
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
            build_flow_fields,
            build_formations,
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
        w.insert_resource(FlowField::default());
        w.insert_resource(Formations::default());
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
    fn melee_attacks_a_single_enemy_not_all_neighbors() {
        // One infantry flanked by two enemies should deal its damage to exactly
        // one of them — total damage dealt == one attack, not two.
        let mut w = fresh_world();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1));
        let a = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1)).id();
        let b = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, -1), 1)).id();

        step(&mut w);

        let max = max_hp(Kind::Infantry);
        let dmg_a = max - w.get::<Health>(a).expect("a alive").0;
        let dmg_b = max - w.get::<Health>(b).expect("b alive").0;
        assert_eq!(
            dmg_a + dmg_b,
            attack_damage(Kind::Infantry),
            "red must land exactly one melee attack, not one per neighbor (a={dmg_a}, b={dmg_b})"
        );
        assert!(
            (dmg_a == 0.0) ^ (dmg_b == 0.0),
            "exactly one neighbor should be struck (a={dmg_a}, b={dmg_b})"
        );
    }

    #[test]
    fn melee_focus_fire_targets_the_weakest_enemy() {
        // Among two adjacent enemies, the already-wounded one is finished first.
        let mut w = fresh_world();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1));
        let weak = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(1, 0), 1)).id();
        let strong = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, -1), 1)).id();
        w.get_mut::<Health>(weak).expect("weak alive").0 = 20.0;

        step(&mut w);

        assert!(
            w.get::<Health>(weak).expect("weak alive").0 < 20.0,
            "the wounded enemy should be focused"
        );
        assert_eq!(
            w.get::<Health>(strong).expect("strong alive").0,
            max_hp(Kind::Infantry),
            "the healthy enemy should be left untouched while the weak one is focused"
        );
    }

    #[test]
    fn surrounded_unit_takes_more_than_it_deals() {
        // A lone unit ringed by six enemies is struck by all of them but strikes
        // back at only one — flanking is lethal.
        let mut w = fresh_world();
        let victim = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, 0), 1)).id();
        let ring: Vec<Entity> = Hex::new(0, 0)
            .neighbors()
            .iter()
            .map(|n| w.spawn(unit(Team::Blue, Kind::Infantry, *n, 1)).id())
            .collect();

        step(&mut w);

        let max = max_hp(Kind::Infantry);
        let dealt: f32 = ring
            .iter()
            .map(|e| max - w.get::<Health>(*e).map(|h| h.0).unwrap_or(0.0))
            .sum();
        let taken = max - w.get::<Health>(victim).expect("victim alive").0;
        assert_eq!(dealt, attack_damage(Kind::Infantry), "victim strikes back at exactly one foe");
        assert!(taken > dealt, "a surrounded unit takes far more than it deals (took {taken}, dealt {dealt})");
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
    fn a_blocked_unit_sidesteps_instead_of_deadlocking() {
        let mut w = fresh_world();
        // Blue marches toward the −q enemy line. A held friendly sits squarely in
        // the only distance-reducing hex (−1,0) directly ahead of the rear unit at
        // (0,0). Pre-sidestep the rear unit would freeze; it must now peel laterally
        // to a hex that holds its distance to the goal (not backward, not onto the
        // blocker).
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::March);
        w.resource_mut::<Orders>().set(Team::Blue, 2, Order::Hold);
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(-1, 0), 2)); // the blocker
        let rear = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, 0), 1)).id();

        let goal_d = |h: Hex| h.distance(enemy_line(Team::Blue));
        let before = goal_d(Hex::new(0, 0));

        step(&mut w);

        let h = *w.get::<Hex>(rear).expect("rear alive");
        assert_ne!(h, Hex::new(0, 0), "boxed-in unit should peel sideways, not freeze");
        assert_ne!(h, Hex::new(-1, 0), "must not step onto the blocker");
        assert!(
            goal_d(h) <= before,
            "sidestep must hold (not lose) ground toward the goal: {h:?}"
        );
    }

    #[test]
    fn a_unit_does_not_sidestep_backward_when_fully_boxed() {
        let mut w = fresh_world();
        // Box the marcher in toward its goal: the three −q-ward neighbors of (0,0)
        // — (−1,0), (−1,1), (0,−1) — are the only hexes that don't increase the
        // distance to the (−30,0) line; wall them all. The unit must stay put
        // rather than sidestep onto a strictly-farther hex.
        w.resource_mut::<Orders>().set(Team::Blue, 1, Order::March);
        {
            let mut t = w.resource_mut::<TerrainMap>();
            t.set(Hex::new(-1, 0), Terrain::Mountain);
            t.set(Hex::new(-1, 1), Terrain::Mountain);
            t.set(Hex::new(0, -1), Terrain::Mountain);
        }
        let blue = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, 0), 1)).id();

        step(&mut w);

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert_eq!(h, Hex::new(0, 0), "fully boxed unit must not retreat sideways, at {h:?}");
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

    /// Build only the spatial index on the current world (no movement/combat),
    /// so the dense grid can be inspected directly.
    fn build_index(w: &mut World) {
        let mut sched = Schedule::default();
        sched.add_systems(build_spatial_index);
        sched.run(w);
    }

    #[test]
    fn dense_index_locates_units_and_misses_elsewhere() {
        let mut w = fresh_world();
        let red = w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(2, -3), 1)).id();
        w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(-1, 4), 1));

        build_index(&mut w);

        let idx = w.resource::<SpatialIndex>();
        assert_eq!(idx.at(Hex::new(2, -3)).map(|(e, _)| e), Some(red), "red must be indexed");
        assert_eq!(idx.at(Hex::new(-1, 4)).map(|(_, t)| t), Some(Team::Blue), "blue must be indexed");
        assert!(!idx.occupied(Hex::new(0, 0)), "an empty interior cell must miss");
        assert!(idx.at(Hex::new(100, 100)).is_none(), "an out-of-bounds cell must miss");
    }

    #[test]
    fn dense_index_handles_an_empty_world() {
        let mut w = fresh_world();
        build_index(&mut w);
        let idx = w.resource::<SpatialIndex>();
        assert!(!idx.occupied(Hex::new(0, 0)), "empty world: every lookup misses");
        assert!(idx.at(Hex::new(5, 5)).is_none());
    }

    #[test]
    fn dense_index_finds_every_unit_at_scale() {
        // Stress/smoke: a 60×60 block (3600 units) on distinct hexes. Every one
        // must be locatable, exercising the dense grid's bounding-box sizing and
        // O(1) indexing at scale without a HashMap.
        let mut w = fresh_world();
        let mut placed = Vec::new();
        for q in 0..60 {
            for r in 0..60 {
                let h = Hex::new(q, r);
                let e = w.spawn(unit(Team::Red, Kind::Infantry, h, 1)).id();
                placed.push((e, h));
            }
        }

        build_index(&mut w);

        let idx = w.resource::<SpatialIndex>();
        for (e, h) in placed {
            assert_eq!(idx.at(h).map(|(x, _)| x), Some(e), "unit at {h:?} must be indexed");
        }
    }

    /// Fill an axial rectangle with Plains, then wall `q = wall_q` with Mountain
    /// for every row except `gap_r` — a one-hex pass through an otherwise solid
    /// barrier. The far side is only reachable around the gap. Goal cells for
    /// `enemy_line` sit inside the rectangle so the field connects end to end.
    fn walled_field(w: &mut World, wall_q: i32, gap_r: i32) {
        let mut t = w.resource_mut::<TerrainMap>();
        for q in -31..=3 {
            for r in -3..=3 {
                t.set(Hex::new(q, r), Terrain::Plains);
            }
        }
        for r in -3..=3 {
            if r != gap_r {
                t.set(Hex::new(wall_q, r), Terrain::Mountain);
            }
        }
    }

    #[test]
    fn flow_step_escapes_a_greedy_dead_end() {
        // A unit pressed against the wall at (2,0) has no neighbor that reduces
        // straight-line distance to the enemy line without crossing a mountain,
        // so the greedy step dead-ends. The flow field knows the only route is up
        // toward the gap, so it still yields a (strictly closer) step.
        let mut w = fresh_world();
        walled_field(&mut w, 1, 3);
        let terrain = w.remove_resource::<TerrainMap>().unwrap();
        let idx = SpatialIndex::default();
        let reserved = HashSet::new();

        let mut field = FlowField::default();
        field
            .dist
            .insert(Team::Blue, integrate(&terrain, enemy_line(Team::Blue)));

        let from = Hex::new(2, 0);
        assert!(
            greedy_step(from, enemy_line(Team::Blue), &terrain, &idx, &reserved).is_none(),
            "greedy must dead-end against the wall"
        );

        let dist = field.dist.get(&Team::Blue).unwrap();
        let step = flow_step(&field, Team::Blue, from, &terrain, &idx, &reserved)
            .expect("flow field must offer an escape step");
        assert!(
            dist[&(step.q, step.r)] < dist[&(from.q, from.r)],
            "flow step must descend the gradient: {step:?}"
        );
        assert!(step.r > from.r, "the only route is up toward the gap: {step:?}");
    }

    #[test]
    fn flow_field_routes_around_a_wall_with_no_enemy_in_sight() {
        // No enemy in vision → the greedy-to-line fallback alone would stall at
        // the wall. With the flow field the unit threads the gap and reaches the
        // far side, never standing on a mountain.
        let mut w = fresh_world();
        walled_field(&mut w, 1, 3);
        let blue = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(3, 0), 1)).id();

        let mut crossed_onto_mountain = false;
        for _ in 0..40 {
            step(&mut w);
            let h = *w.get::<Hex>(blue).expect("blue alive");
            if w.resource::<TerrainMap>().get(h) == Terrain::Mountain {
                crossed_onto_mountain = true;
            }
        }

        let h = *w.get::<Hex>(blue).expect("blue alive");
        assert!(!crossed_onto_mountain, "unit must never stand on a mountain");
        assert!(
            h.q <= 0,
            "unit should round the wall and reach the far side, at {h:?}"
        );
    }

    #[test]
    fn cohesion_holds_back_a_unit_that_outran_its_group() {
        // A Blue group: most of the block sits deep at q=20, but one runner has
        // pushed far ahead to q=2 (much closer to Red's line at q=-30). No enemy
        // is anywhere in vision, so the runner should pause for cohesion while
        // the trailing block keeps advancing — shrinking the spread.
        let mut w = fresh_world();
        let runner = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(2, 0), 1)).id();
        let mut rear = Vec::new();
        for r in 0..6 {
            rear.push(w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(20, r), 1)).id());
        }

        let runner_before = w.get::<Hex>(runner).unwrap().q;
        step(&mut w);

        let runner_after = w.get::<Hex>(runner).unwrap().q;
        assert_eq!(
            runner_after, runner_before,
            "the runner is far ahead of its group's front and must hold for cohesion"
        );
        // ...while a rear unit (behind the mean) advances to close the gap.
        let rear_after = w.get::<Hex>(rear[0]).unwrap().q;
        assert!(rear_after < 20, "a trailing unit should keep advancing, at q={rear_after}");
    }

    #[test]
    fn a_lagging_unit_still_advances_toward_the_front() {
        // Mirror image: the unit under test is *behind* its group's front, so it
        // must advance (catch up), never pause.
        let mut w = fresh_world();
        for r in 0..6 {
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(2, r), 1));
        }
        let laggard = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(20, 0), 1)).id();

        step(&mut w);

        let q = w.get::<Hex>(laggard).unwrap().q;
        assert!(q < 20, "a lagging unit must advance to catch its group, at q={q}");
    }

    #[test]
    fn cohesion_never_freezes_a_lone_unit() {
        // Regression guard for `isolated_unit_marches_*`: a group of one is its
        // own front, so cohesion can never apply and it must still advance.
        let mut w = fresh_world();
        let solo = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(10, 0), 1)).id();

        step(&mut w);

        let q = w.get::<Hex>(solo).unwrap().q;
        assert!(q < 10, "a lone unit must keep marching, at q={q}");
    }

    #[test]
    fn cohesion_yields_to_a_visible_enemy() {
        // The runner is far ahead of its group's front, but an enemy sits within
        // vision. Contact overrides cohesion: it must engage, not hold for ranks.
        let mut w = fresh_world();
        let runner = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(2, 0), 1)).id();
        w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(-4, 0), 1)); // within VISION of the runner
        for r in 0..6 {
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(20, r), 1));
        }

        let before = *w.get::<Hex>(runner).unwrap();
        step(&mut w);

        let after = *w.get::<Hex>(runner).unwrap();
        assert_ne!(after, before, "a unit that can see an enemy must move despite cohesion");
        assert!(after.distance(Hex::new(-4, 0)) < before.distance(Hex::new(-4, 0)),
            "it should close on the visible enemy, at {after:?}");
    }

    #[test]
    fn line_to_is_contiguous_and_hits_both_endpoints() {
        let a = Hex::new(0, 0);
        let b = Hex::new(3, -1);
        let line = a.line_to(b);
        assert_eq!(line.first(), Some(&a));
        assert_eq!(line.last(), Some(&b));
        assert_eq!(line.len() as i32, a.distance(b) + 1, "one hex per step plus the start");
        // Each consecutive pair is adjacent (distance 1).
        for pair in line.windows(2) {
            assert_eq!(pair[0].distance(pair[1]), 1, "line must be contiguous: {pair:?}");
        }
    }

    #[test]
    fn mountains_block_line_of_sight_but_endpoints_do_not() {
        let mut w = fresh_world();
        let from = Hex::new(0, 0);
        let to = Hex::new(3, 0);
        // Clear line first.
        assert!(line_of_sight(from, to, &w.resource::<TerrainMap>()), "plains is clear");
        // A mountain strictly between blocks.
        w.resource_mut::<TerrainMap>().set(Hex::new(2, 0), Terrain::Mountain);
        assert!(!line_of_sight(from, to, &w.resource::<TerrainMap>()), "intervening mountain blocks");
        // A mountain on the *target* hex does not block (endpoints excluded).
        let mut clear = fresh_world();
        clear.resource_mut::<TerrainMap>().set(to, Terrain::Mountain);
        assert!(line_of_sight(from, to, &clear.resource::<TerrainMap>()), "target on a mountain is still visible");
    }

    #[test]
    fn skirmishers_cannot_shoot_through_a_mountain() {
        let mut w = fresh_world();
        // Wall the line between shooter (0,0) and target (3,0).
        {
            let mut t = w.resource_mut::<TerrainMap>();
            t.set(Hex::new(1, 0), Terrain::Mountain);
            t.set(Hex::new(2, 0), Terrain::Mountain);
        }
        w.spawn(unit(Team::Red, Kind::Skirmisher, Hex::new(0, 0), 1));
        let blue = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(3, 0), 1)).id();

        step(&mut w);

        let hp = w.get::<Health>(blue).expect("blue alive").0;
        assert_eq!(hp, max_hp(Kind::Infantry), "shot must be blocked by the mountain, hp={hp}");
    }

    #[test]
    fn skirmishers_shoot_a_visible_foe_past_an_obstructed_one() {
        let mut w = fresh_world();
        w.resource_mut::<TerrainMap>().set(Hex::new(1, 0), Terrain::Mountain);
        w.spawn(unit(Team::Red, Kind::Skirmisher, Hex::new(0, 0), 1));
        // Obstructed enemy directly behind the mountain (distance 2).
        let hidden = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(2, 0), 1)).id();
        // Equally-close enemy on a clear line (distance 2).
        let visible = w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(0, 2), 1)).id();

        step(&mut w);

        let hidden_hp = w.get::<Health>(hidden).expect("hidden alive").0;
        let visible_hp = w.get::<Health>(visible).expect("visible alive").0;
        assert_eq!(hidden_hp, max_hp(Kind::Infantry), "hidden foe must be safe behind the mountain");
        assert!(visible_hp < max_hp(Kind::Infantry), "the unobstructed foe should be shot, hp={visible_hp}");
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

    // --- Robustness: determinism, invariants, and edge-case panic-safety ---

    /// Order-independent snapshot of the live battle state. Two runs that agree
    /// here agree on everything observable (team, position, exact HP bits).
    fn snapshot(w: &mut World) -> Vec<(u8, i32, i32, u32)> {
        let mut q = w.query::<(&Team, &Hex, &Health)>();
        let mut v: Vec<(u8, i32, i32, u32)> = q
            .iter(w)
            .map(|(t, h, hp)| ((*t == Team::Blue) as u8, h.q, h.r, hp.0.to_bits()))
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn simulation_is_bit_for_bit_deterministic() {
        // The whole testable-engine premise rests on this: identical inputs must
        // produce an identical trajectory, tick for tick, on every run.
        let build = || {
            let mut w = fresh_world();
            for r in 0..6 {
                w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(0, r), 1));
                w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(3, r), 1));
                w.spawn(unit(Team::Blue, Kind::Skirmisher, Hex::new(5, r), 1));
            }
            w
        };
        let mut a = build();
        let mut b = build();
        for t in 0..200 {
            step(&mut a);
            step(&mut b);
            assert_eq!(
                snapshot(&mut a),
                snapshot(&mut b),
                "diverged at tick {t}: identical inputs must stay identical"
            );
        }
    }

    #[test]
    fn battle_preserves_core_invariants_every_tick() {
        let mut w = fresh_world();
        // A mountain ridge at q=0 the armies must route around, never onto.
        {
            let mut t = w.resource_mut::<TerrainMap>();
            for r in -3..=3 {
                t.set(Hex::new(0, r), Terrain::Mountain);
            }
        }
        for r in -4..=4 {
            w.spawn(unit(Team::Red, Kind::Cavalry, Hex::new(-3, r), 1));
            w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(3, r), 1));
        }

        let mut prev = usize::MAX;
        for tick in 0..150 {
            step(&mut w);

            let snap: Vec<(Hex, f32, Kind)> = {
                let mut q = w.query::<(&Hex, &Health, &Kind)>();
                q.iter(&w).map(|(h, hp, k)| (*h, hp.0, *k)).collect()
            };
            let terrain = w.resource::<TerrainMap>();
            let mut seen: HashSet<(i32, i32)> = HashSet::new();
            for (h, hp, k) in &snap {
                // Health stays finite, positive (the dead are despawned), and is
                // never healed above the unit's cap.
                assert!(
                    hp.is_finite() && *hp > 0.0 && *hp <= max_hp(*k),
                    "tick {tick}: HP out of range: {hp} for {k:?}"
                );
                // Units never stand on impassable terrain.
                assert!(
                    terrain.get(*h).passable(),
                    "tick {tick}: unit on impassable terrain at {h:?}"
                );
                // The rigid-block invariant: at most one unit per hex.
                assert!(
                    seen.insert((h.q, h.r)),
                    "tick {tick}: two units share hex {h:?}"
                );
            }
            // No unit is ever spawned mid-battle; the count only falls.
            assert!(snap.len() <= prev, "tick {tick}: unit count grew to {}", snap.len());
            prev = snap.len();
        }
    }

    // --- ring-scan nearest-enemy probe -------------------------------------

    /// Build a spatial index directly from `(hex, team)` pairs. Entities are
    /// placeholders — the scan only reads position/team and returns them.
    fn idx_from(cells: &[(Hex, Team)]) -> SpatialIndex {
        let mut idx = SpatialIndex::default();
        if let (Some(q_min), Some(q_max), Some(r_min), Some(r_max)) = (
            cells.iter().map(|(h, _)| h.q).min(),
            cells.iter().map(|(h, _)| h.q).max(),
            cells.iter().map(|(h, _)| h.r).min(),
            cells.iter().map(|(h, _)| h.r).max(),
        ) {
            idx.reset(Some((q_min, r_min, q_max, r_max)));
            for (h, t) in cells {
                idx.set(*h, Entity::PLACEHOLDER, *t);
            }
        }
        idx
    }

    #[test]
    fn ring_scan_returns_an_enemy_at_the_minimum_distance() {
        let from = Hex::new(0, 0);
        // Three enemies at distances 5, 2 and 3 — the nearest is at distance 2.
        let idx = idx_from(&[
            (Hex::new(5, 0), Team::Blue),
            (Hex::new(2, -1), Team::Blue),
            (Hex::new(0, 3), Team::Blue),
        ]);
        let got = nearest_enemy(&idx, from, Team::Red).expect("an enemy is in range");
        assert_eq!(from.distance(got), 2, "must return a closest enemy, got {got:?}");
    }

    #[test]
    fn ring_scan_visits_every_cell_of_a_ring() {
        // For each cell at distance 2, a lone enemy placed there must be found —
        // proving the ring walk reaches the whole ring (no gaps).
        let from = Hex::new(0, 0);
        for dq in -2..=2 {
            for dr in -2..=2 {
                let h = Hex::new(dq, dr);
                if from.distance(h) != 2 {
                    continue;
                }
                let idx = idx_from(&[(h, Team::Blue)]);
                assert_eq!(
                    nearest_enemy(&idx, from, Team::Red),
                    Some(h),
                    "ring scan missed {h:?}"
                );
            }
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

    #[test]
    fn ring_scan_matches_a_brute_force_minimum() {
        // A scattered field (incl. equidistant ties and own-team decoys); the
        // returned enemy must sit at the true minimum distance, and the team
        // filter must skip friendlies.
        let from = Hex::new(0, 0);
        let field = [
            (Hex::new(4, 0), Team::Blue),
            (Hex::new(-3, 1), Team::Blue),
            (Hex::new(2, 1), Team::Blue),  // dist 3
            (Hex::new(-2, -1), Team::Blue), // dist 3 (tie)
            (Hex::new(1, 0), Team::Red),   // friendly, must be ignored
        ];
        let idx = idx_from(&field);
        let got = nearest_enemy(&idx, from, Team::Red).expect("enemy in range");
        let min_d = field
            .iter()
            .filter(|(_, t)| *t == Team::Blue)
            .map(|(h, _)| from.distance(*h))
            .min()
            .unwrap();
        assert_eq!(from.distance(got), min_d, "got {got:?}, expected distance {min_d}");
        assert_ne!(got, Hex::new(1, 0), "must not target a friendly");
    }

    #[test]
    fn ring_scan_ignores_enemies_beyond_range() {
        let from = Hex::new(0, 0);
        // Only enemy sits past VISION → out of range, nothing found.
        let idx = idx_from(&[(Hex::new(VISION + 3, 0), Team::Blue)]);
        assert_eq!(nearest_enemy(&idx, from, Team::Red), None);
    }

    #[test]
    fn ring_scan_entity_targets_the_closest_foe() {
        // Distinct entities: the ranged target must be the nearer one.
        let mut w = World::new();
        let near = w.spawn((Hex::new(1, 0), Team::Blue)).id();
        let far = w.spawn((Hex::new(3, 0), Team::Blue)).id();
        let mut idx = SpatialIndex::default();
        idx.reset(Some((1, 0, 3, 0)));
        idx.set(Hex::new(1, 0), near, Team::Blue);
        idx.set(Hex::new(3, 0), far, Team::Blue);
        assert_eq!(
            nearest_enemy_entity(&idx, Hex::new(0, 0), Team::Red, 3, &TerrainMap::default()),
            Some(near),
            "ranged attack must pick the nearest enemy"
        );
    }

    #[test]
    fn empty_world_steps_without_panic() {
        // A degenerate world (no units, no terrain) must tick cleanly.
        let mut w = fresh_world();
        for _ in 0..10 {
            step(&mut w);
        }
        assert_eq!(w.resource::<Tick>().0, 10);
    }

    #[test]
    fn a_fully_walled_unit_stays_put_without_panic() {
        let mut w = fresh_world();
        {
            let mut t = w.resource_mut::<TerrainMap>();
            for n in Hex::new(0, 0).neighbors() {
                t.set(n, Terrain::Mountain);
            }
        }
        let u = w.spawn(unit(Team::Blue, Kind::Cavalry, Hex::new(0, 0), 1)).id();

        for _ in 0..10 {
            step(&mut w);
        }

        let h = *w.get::<Hex>(u).expect("unit alive");
        assert_eq!(h, Hex::new(0, 0), "a boxed-in unit has no legal move, at {h:?}");
    }

    #[test]
    fn ai_order_balance_math_survives_scale() {
        // `own * 5` overflows a u32 past ~858M units — a debug-build panic.
        // The balance thresholds must still hold at hundreds of millions.
        assert_eq!(ai_order(1_000_000_000, 0, 0), Order::March, "no enemy → advance");
        assert_eq!(
            ai_order(1_000_000_000, 1_000_000_000, 5),
            Order::Charge,
            "even and engaged at scale → launch"
        );
        assert_eq!(
            ai_order(500_000_000, 1_000_000_000, 5),
            Order::Hold,
            "outnumbered at scale → defend"
        );
    }

    #[test]
    fn proximity_scan_holds_up_at_scale() {
        // Stress/smoke for the hot scan: two ~600-unit blocks collide for 40
        // ticks. Asserts panic-free and a monotonically non-increasing count —
        // every moving unit runs the ring scan each tick.
        let mut w = fresh_world();
        for q in 0..30 {
            for r in 0..20 {
                w.spawn(unit(Team::Red, Kind::Infantry, Hex::new(q, r), 1));
                w.spawn(unit(Team::Blue, Kind::Infantry, Hex::new(q + 31, r), 1));
            }
        }
        let count = |w: &mut World| w.query::<&Team>().iter(w).count();
        let mut prev = count(&mut w);
        for _ in 0..40 {
            step(&mut w);
            let now = count(&mut w);
            assert!(now <= prev, "unit count must not grow: {prev} -> {now}");
            prev = now;
        }
    }
}
