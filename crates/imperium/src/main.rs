//! Imperium — Fase 1 spike.
//!
//! A Bevy 0.18 window that runs the pure `sim_core` battle on a fixed 2 Hz tick
//! and renders each unit as a colored hexagon. Two infantry blocks advance,
//! clash, and one side is wiped. Press 1/2/3 to order the RED army to
//! March / Charge / Hold. All logic lives in `sim_core`; the renderer just
//! mirrors `Hex` → `Transform` each frame.

use bevy::diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin};
use bevy::prelude::*;
use bevy::remote::{http::RemoteHttpPlugin, RemotePlugin};
use sim_core::{
    generate_terrain, unit, AnimCatalog, AnimState, BattleEvents, DamageBuffer, FlowField,
    Formations, Group, Health, Hex, Kind, MovedThisTick, NextMove, Order, Orders, SpatialIndex,
    Team, Terrain, TerrainMap, Tick,
};

const HEX_SIZE: f32 = 12.0;
const ARMY_COLS: i32 = 28;
const ARMY_ROWS: i32 = 54;
const GAP: i32 = 12; // hexes between the two armies' inner edges
const GRID_Q: i32 = 40;
const GRID_R: i32 = 34;
const SEED: i32 = 7;
/// Rotate a Bevy `RegularPolygon` (pointy-top by default) to flat-top.
const FLAT_TOP: f32 = std::f32::consts::FRAC_PI_6;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        // Bevy Remote Protocol: query/mutate the live ECS over JSON-RPC (port
        // 15702). This is the runtime-agent hook — an agent can drive/inspect
        // the running battle. Register the sim components so they're queryable.
        .add_plugins((RemotePlugin::default(), RemoteHttpPlugin::default()))
        // FPS + frame time logged to console every second (scale stress test).
        .add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            LogDiagnosticsPlugin::default(),
        ))
        .register_type::<Hex>()
        .register_type::<Health>()
        .register_type::<Team>()
        .register_type::<Kind>()
        .register_type::<Group>()
        .register_type::<NextMove>()
        .register_type::<AnimState>()
        .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.07)))
        .insert_resource(generate_terrain(SEED, GRID_Q, GRID_R))
        // Battle sim runs on a fixed timestep, decoupled from render framerate.
        .insert_resource(Time::<Fixed>::from_hz(2.0))
        .insert_resource(Tick::default())
        .insert_resource(Orders::default())
        .insert_resource(SpatialIndex::default())
        .insert_resource(DamageBuffer::default())
        // Animation data layer: per-tick event log + mover set drive AnimState,
        // and the catalog tells the render crate which clip each state maps to.
        .insert_resource(BattleEvents::default())
        .insert_resource(MovedThisTick::default())
        .insert_resource(AnimCatalog::default())
        .insert_resource(FlowField::default())
        .insert_resource(Formations::default())
        .add_systems(Startup, setup)
        .add_systems(
            FixedUpdate,
            (
                sim_core::tick_and_clear,
                sim_core::build_spatial_index,
                sim_core::build_flow_fields,
                sim_core::build_formations,
                sim_core::enemy_ai,
                sim_core::combat,
                sim_core::resolve_damage,
                sim_core::movement,
                sim_core::animate,
                log_status,
            )
                .chain(),
        )
        .add_systems(Update, (control, sync_transforms))
        .run();
}

fn terrain_color(t: Terrain) -> Color {
    match t {
        Terrain::Plains => Color::srgb(0.20, 0.28, 0.17),
        Terrain::Forest => Color::srgb(0.11, 0.21, 0.12),
        Terrain::Hill => Color::srgb(0.36, 0.29, 0.18),
        Terrain::Mountain => Color::srgb(0.36, 0.36, 0.40),
        Terrain::Water => Color::srgb(0.11, 0.21, 0.42),
    }
}

/// Flat-top axial → world pixels.
fn hex_to_world(h: Hex) -> Vec2 {
    let x = HEX_SIZE * 1.5 * h.q as f32;
    let y = HEX_SIZE * 3.0_f32.sqrt() * (h.r as f32 + h.q as f32 / 2.0);
    Vec2::new(x, -y)
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut terrain: ResMut<TerrainMap>,
) {
    commands.spawn(Camera2d);

    // Carve the two deploy zones to Plains so units never spawn stuck.
    for col in 0..ARMY_COLS {
        for row in 0..ARMY_ROWS {
            let r = row - ARMY_ROWS / 2;
            terrain.set(Hex::new(-(GAP / 2) - 1 - col, r), Terrain::Plains);
            terrain.set(Hex::new((GAP / 2) + 1 + col, r), Terrain::Plains);
        }
    }

    // Static hex grid, flat-top tiles inset so the dark clear color reads as
    // grid lines. Each tile colored by its terrain.
    let grid_mesh = meshes.add(RegularPolygon::new(HEX_SIZE * 0.92, 6));
    let mats: Vec<(Terrain, Handle<ColorMaterial>)> = [
        Terrain::Plains,
        Terrain::Forest,
        Terrain::Hill,
        Terrain::Mountain,
        Terrain::Water,
    ]
    .into_iter()
    .map(|t| (t, materials.add(terrain_color(t))))
    .collect();
    let mat_for = |t: Terrain| mats.iter().find(|(k, _)| *k == t).unwrap().1.clone();

    for q in -GRID_Q..=GRID_Q {
        for r in -GRID_R..=GRID_R {
            let p = hex_to_world(Hex::new(q, r));
            commands.spawn((
                Mesh2d(grid_mesh.clone()),
                MeshMaterial2d(mat_for(terrain.get(Hex::new(q, r)))),
                Transform::from_xyz(p.x, p.y, -1.0).with_rotation(Quat::from_rotation_z(FLAT_TOP)),
            ));
        }
    }

    let mesh = meshes.add(RegularPolygon::new(HEX_SIZE * 0.6, 6));
    // One material per (team, kind): team hue, brightness by kind.
    let mut umat: Vec<(Team, Kind, Handle<ColorMaterial>)> = Vec::new();
    for team in [Team::Red, Team::Blue] {
        for kind in [Kind::Infantry, Kind::Cavalry, Kind::Skirmisher] {
            umat.push((team, kind, materials.add(unit_color(team, kind))));
        }
    }
    let mat_for = |team, kind| umat.iter().find(|(t, k, _)| *t == team && *k == kind).unwrap().2.clone();

    // Red on the left, Blue on the right; a gap in the middle. Cavalry forms
    // the front (inner columns), infantry the centre, skirmishers the rear.
    let mut n = 0;
    for col in 0..ARMY_COLS {
        let kind = kind_for(col, ARMY_COLS);
        for row in 0..ARMY_ROWS {
            let r = row - ARMY_ROWS / 2;
            let (rq, bq) = (-(GAP / 2) - 1 - col, (GAP / 2) + 1 + col);
            spawn_unit(&mut commands, &mesh, &mat_for(Team::Red, kind), Team::Red, kind, Hex::new(rq, r));
            spawn_unit(&mut commands, &mesh, &mat_for(Team::Blue, kind), Team::Blue, kind, Hex::new(bq, r));
            n += 2;
        }
    }

    info!("spawned {n} units | controls: [1] Red March  [2] Red Charge  [3] Red Hold");
}

fn spawn_unit(
    commands: &mut Commands,
    mesh: &Handle<Mesh>,
    material: &Handle<ColorMaterial>,
    team: Team,
    kind: Kind,
    hex: Hex,
) {
    let p = hex_to_world(hex);
    commands.spawn((
        unit(team, kind, hex, 1),
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Transform::from_xyz(p.x, p.y, 0.0).with_rotation(Quat::from_rotation_z(FLAT_TOP)),
    ));
}

/// Front line is cavalry, centre infantry, rear skirmishers — by how deep the
/// column sits in the formation (col 0 = inner/front).
fn kind_for(col: i32, cols: i32) -> Kind {
    if col < cols / 4 {
        Kind::Cavalry
    } else if col >= cols * 3 / 4 {
        Kind::Skirmisher
    } else {
        Kind::Infantry
    }
}

fn unit_color(team: Team, kind: Kind) -> Color {
    let (r, g, b): (f32, f32, f32) = match team {
        Team::Red => (0.92, 0.30, 0.30),
        Team::Blue => (0.34, 0.52, 0.96),
    };
    let f: f32 = match kind {
        Kind::Cavalry => 1.25,
        Kind::Infantry => 1.0,
        Kind::Skirmisher => 0.65,
    };
    Color::srgb((r * f).min(1.0), (g * f).min(1.0), (b * f).min(1.0))
}

/// Keyboard → orders for the Red army (group 1).
fn control(keys: Res<ButtonInput<KeyCode>>, mut orders: ResMut<Orders>) {
    if keys.just_pressed(KeyCode::Digit1) {
        orders.set(Team::Red, 1, Order::March);
        info!("Red → March");
    }
    if keys.just_pressed(KeyCode::Digit2) {
        orders.set(Team::Red, 1, Order::Charge);
        info!("Red → Charge");
    }
    if keys.just_pressed(KeyCode::Digit3) {
        orders.set(Team::Red, 1, Order::Hold);
        info!("Red → Hold");
    }
    if keys.just_pressed(KeyCode::Digit4) {
        orders.set(Team::Red, 1, Order::Retreat);
        info!("Red → Retreat");
    }
    if keys.just_pressed(KeyCode::Digit5) {
        orders.set(Team::Red, 1, Order::Unleash);
        info!("Red → Unleash");
    }
}

/// Mirror the sim's authoritative `Hex` onto the render `Transform` each frame.
fn sync_transforms(mut q: Query<(&Hex, &mut Transform)>) {
    for (h, mut t) in &mut q {
        let p = hex_to_world(*h);
        t.translation.x = p.x;
        t.translation.y = p.y;
    }
}

fn log_status(tick: Res<Tick>, orders: Res<Orders>, q: Query<&Team>) {
    if tick.0 % 4 != 0 {
        return;
    }
    let (mut red, mut blue) = (0, 0);
    for t in &q {
        match t {
            Team::Red => red += 1,
            Team::Blue => blue += 1,
        }
    }
    info!(
        "tick {:>4} | red {:>3} ({:?}) | blue {:>3} (AI {:?})",
        tick.0,
        red,
        orders.get(Team::Red, 1),
        blue,
        orders.get(Team::Blue, 1),
    );
}
