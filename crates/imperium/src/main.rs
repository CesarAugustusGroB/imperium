//! Imperium — Fase 1 spike.
//!
//! A Bevy 0.18 window that runs the pure `sim_core` battle on a fixed 2 Hz tick
//! and renders each unit as a colored hexagon. Two infantry blocks advance,
//! clash, and one side is wiped. Press 1/2/3 to order the RED army to
//! March / Charge / Hold. All logic lives in `sim_core`; the renderer just
//! mirrors `Hex` → `Transform` each frame.

use bevy::prelude::*;
use sim_core::{
    generate_terrain, unit, DamageBuffer, Hex, Kind, Order, Orders, SpatialIndex, Team, Terrain,
    TerrainMap, Tick,
};

const HEX_SIZE: f32 = 12.0;
const COLS: i32 = 10;
const ROWS: i32 = 14;
const GRID_Q: i32 = 18;
const GRID_R: i32 = 11;
const SEED: i32 = 7;
/// Rotate a Bevy `RegularPolygon` (pointy-top by default) to flat-top.
const FLAT_TOP: f32 = std::f32::consts::FRAC_PI_6;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.07)))
        .insert_resource(generate_terrain(SEED, GRID_Q, GRID_R))
        // Battle sim runs on a fixed timestep, decoupled from render framerate.
        .insert_resource(Time::<Fixed>::from_hz(2.0))
        .insert_resource(Tick::default())
        .insert_resource(Orders::default())
        .insert_resource(SpatialIndex::default())
        .insert_resource(DamageBuffer::default())
        .add_systems(Startup, setup)
        .add_systems(
            FixedUpdate,
            (
                sim_core::tick_and_clear,
                sim_core::build_spatial_index,
                sim_core::combat,
                sim_core::resolve_damage,
                sim_core::movement,
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
    for r in -(ROWS / 2)..=(ROWS / 2 - 1) {
        for col in 0..COLS {
            terrain.set(Hex::new(-15 + col, r), Terrain::Plains);
            terrain.set(Hex::new(6 + col, r), Terrain::Plains);
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
    let red = materials.add(Color::srgb(0.92, 0.30, 0.30));
    let blue = materials.add(Color::srgb(0.34, 0.52, 0.96));

    // Red block on the left, Blue block on the right; a gap in the middle.
    for col in 0..COLS {
        for row in 0..ROWS {
            let r = row - ROWS / 2;
            spawn_unit(&mut commands, &mesh, &red, Team::Red, Hex::new(-15 + col, r));
            spawn_unit(&mut commands, &mesh, &blue, Team::Blue, Hex::new(6 + col, r));
        }
    }

    info!("controls: [1] Red March  [2] Red Charge  [3] Red Hold");
}

fn spawn_unit(
    commands: &mut Commands,
    mesh: &Handle<Mesh>,
    material: &Handle<ColorMaterial>,
    team: Team,
    hex: Hex,
) {
    let p = hex_to_world(hex);
    commands.spawn((
        unit(team, Kind::Infantry, hex, 1),
        Mesh2d(mesh.clone()),
        MeshMaterial2d(material.clone()),
        Transform::from_xyz(p.x, p.y, 0.0).with_rotation(Quat::from_rotation_z(FLAT_TOP)),
    ));
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
        "tick {:>4} | red {:>3} ({:?}) | blue {:>3}",
        tick.0,
        red,
        orders.get(Team::Red, 1),
        blue
    );
}
