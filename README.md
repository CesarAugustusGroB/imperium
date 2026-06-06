# Imperium — Fase 0 spike

Prototipo en Bevy del motor del juego fusionado. Valida el stack y el diseño ECS
de la batalla del [blueprint](../hex-tactics/.worktrees/feature-presentation/docs/rust-engine-research/00-ENGINE-BLUEPRINT.md)
antes de comprometer el juego entero.

## Qué hace (Fase 2)

- 280 unidades (140 rojas vs 140 azules) sobre un **mapa hexagonal con terreno**
  (llanura / bosque / colina / montaña / agua) avanzan, lo rodean, chocan y un
  bando es aniquilado.
- **Terreno** con efecto mecánico: montaña/agua intransitables, bosque/colina
  ralentizan (mayor cooldown) y dan **bonus defensivo** (menos daño recibido).
  Generación determinista por semilla (hash noise, sin deps).
- **Órdenes por grupo** (March / Charge / Hold / Idle) y **cooldowns** por tipo;
  charge pega más, hold reduce daño.
- Controles: **`1`** Red March · **`2`** Red Charge · **`3`** Red Hold.
- Toda la lógica vive en `sim_core` (ECS puro sobre `bevy_ecs`, **headless, testeable**:
  7 tests).
- El binario `imperium` (Bevy) corre el sim a **2 ticks/seg** (fixed timestep) y
  renderiza terreno + unidades; el render solo espeja `Hex → Transform`.

## Estructura

```
imperium/
├── Cargo.toml                # workspace
└── crates/
    ├── sim_core/             # batalla pura (bevy_ecs, sin render) + tests
    │   └── src/lib.rs
    └── imperium/             # app Bevy (render, ventana, fixed tick)
        └── src/main.rs
```

## Requisitos

Rust (toolchain MSVC en Windows):

```powershell
winget install Rustlang.Rustup
rustup default stable-msvc
```

> Si `cargo build` falla con un error de `link.exe`, instalá los **C++ Build Tools**:
> `winget install Microsoft.VisualStudio.2022.BuildTools` (workload "Desktop development with C++").

## Correr

```powershell
# desde imperium/
cargo test -p sim_core      # tests headless de la batalla (rápido)
cargo run -p imperium       # abre la ventana con la batalla
```

> La **primera** compilación de Bevy tarda varios minutos (compila todo el engine);
> las siguientes son rápidas. Para iterar aún más rápido, descomentá la feature
> `bevy/dynamic_linking` en `crates/imperium/Cargo.toml`.

## Pendiente

- **Fase 2c — `hexx` + A\***: reemplazar el hex math a mano por `hexx` y rutar con
  A* alrededor de obstáculos (hoy el paso es greedy + skip-impassable, puede
  estancarse en concavidades). Ahora que hay terreno, A* gana.
- `bevy_ecs_tilemap` para tiles texturizados — diferido a cuando haya arte (necesita
  atlas; el grid de mallas coloreadas alcanza por ahora).
- Órdenes restantes (retreat/unleash), tipos ranged (skirmishers).
- Spatial index linked-list sobre arrays (el `HashMap` actual es el placeholder; el
  cambio importa al empujar a miles).
- BRP/MCP para manejar el juego desde agentes; Steamworks.

## Notas de diseño

- `sim_core` depende **solo** de `bevy_ecs` → corre sin ventana ni render. El test
  `battle_resolves_to_a_decided_outcome` construye un `World`, corre 500 ticks y
  assertea — el equivalente Rust del harness `sim-formations.ts`.
- Determinismo: el `Schedule` corre los sistemas con `.chain()` (orden secuencial).
- Las entidades comparten componentes de sim (`Hex`, `Health`, `Team`) y de render
  (`Mesh2d`, ...). Cuando el sim hace `despawn`, el sprite desaparece solo.
