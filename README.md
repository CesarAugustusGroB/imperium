# Imperium — Fase 0 spike

Prototipo en Bevy del motor del juego fusionado. Valida el stack y el diseño ECS
de la batalla del [blueprint](../hex-tactics/.worktrees/feature-presentation/docs/rust-engine-research/00-ENGINE-BLUEPRINT.md)
antes de comprometer el juego entero.

## Qué hace (Fase 3)

- **~3000 unidades** (1512 rojas vs 1512 azules) sobre un **mapa hexagonal con
  terreno** (llanura / bosque / colina / montaña / agua) avanzan, lo rodean, chocan
  y un bando es aniquilado. Corre a **~50-90 FPS en build *debug*** (release: mucho
  más). Bajá `ARMY_COLS`/`ARMY_ROWS` en `main.rs` para un combate más chico/viewable.
- **Tres tipos de unidad** con rol distinto: **infantry** (tanque, melee),
  **cavalry** (rápida, carga fuerte; al frente), **skirmisher** (dispara a distancia
  y *kitea* — se aleja del cuerpo a cuerpo; en retaguardia). Coloreadas por tipo.
- **IA enemiga** (bando azul): setea sus órdenes según el balance de fuerzas —
  avanza para cerrar, *hold* cuando está parejo o perdiendo, *charge* cuando va
  ganando y hay contacto. Emula el estilo "amasar → lanzar / defender perdiendo".
- **BRP** (Bevy Remote Protocol): el ECS se expone por JSON-RPC en el puerto 15702;
  un agente puede leer/mutar el juego corriendo (ver sección *Agentes*).
- **Terreno** con efecto mecánico: montaña/agua intransitables, bosque/colina
  ralentizan (mayor cooldown) y dan **bonus defensivo** (menos daño recibido).
  Generación determinista por semilla (hash noise, sin deps).
- **Pathfinding A\*** (hexx): las unidades rutan alrededor de montañas/agua hacia
  el enemigo visible (greedy en el avance abierto).
- **Órdenes por grupo** (March / Charge / Hold / Idle) y **cooldowns** por tipo;
  charge pega más, hold reduce daño.
- Controles: **`1`** Red March · **`2`** Red Charge · **`3`** Red Hold.
- Toda la lógica vive en `sim_core` (ECS puro sobre `bevy_ecs`, **headless, testeable**:
  17 tests).
- **Proximidad por anillos**: la búsqueda del enemigo más cercano (movimiento y
  tiro) recorre el disco hex **anillo por anillo hacia afuera y corta en el primer
  anillo con un enemigo** — el caso común (enemigo adyacente) toca ~6 celdas en vez
  de la caja completa `(2·VISION+1)²`. Acota el costo del probe por unidad por tick.
- El binario `imperium` (Bevy) corre el sim a **2 ticks/seg** (fixed timestep) y
  renderiza terreno + unidades; el render solo espeja `Hex → Transform`.

## Estructura

```
imperium/
├── Cargo.toml                # workspace
└── crates/
    ├── sim_core/             # batalla pura (bevy_ecs, sin render) + tests
    │   └── src/lib.rs
    ├── imperium/             # app Bevy (render, ventana, fixed tick, BRP)
    │   └── src/main.rs
    └── imperium-mcp/         # MCP server (stdio) → proxy al BRP del juego
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

## Agentes en runtime (BRP)

Con el juego corriendo, el **Bevy Remote Protocol** expone el ECS por JSON-RPC en
`http://127.0.0.1:15702` — un agente puede **leer y manejar** la batalla en vivo.

```powershell
# leer todas las unidades vivas (Team + posición + HP)
$body = '{"jsonrpc":"2.0","id":1,"method":"world.query","params":{"data":{"components":["sim_core::Team","sim_core::Hex","sim_core::Health"]}}}'
Invoke-RestMethod -Uri http://127.0.0.1:15702 -Method Post -ContentType 'application/json' -Body $body
# descubrir todos los métodos: world.query / world.mutate_components / world.spawn_entity / ...
'{"jsonrpc":"2.0","id":1,"method":"rpc.discover"}'
```

> ⚠️ **Gotcha de versión:** en Bevy 0.18 los métodos BRP son `world.*`
> (`world.query`, `world.list_components`, …), **no** `bevy/*` como en 0.15–0.16.
> Los componentes deben derivar `Reflect` + `#[reflect(Component)]` y registrarse
> con `register_type` para ser consultables.

### MCP (manejar el juego como agente)

`crates/imperium-mcp` es un **MCP server (stdio)** que envuelve el BRP con tools:

- `battle_report` — conteo de unidades vivas por bando y por tipo.
- `smite(team)` — mata una unidad del bando dado (efecto visible).

Está registrado en `.mcp.json` (vía `cargo run -p imperium-mcp`). Para usarlo desde
un cliente MCP (Claude Code, etc.): tené el **juego corriendo** (para que el BRP esté
arriba), agregá el `.mcp.json` y **reiniciá la sesión** del cliente (los MCP servers
se cargan al arranque). Verificación manual por stdio:

```powershell
# con el juego corriendo:
$msgs = @(
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}',
 '{"jsonrpc":"2.0","method":"notifications/initialized"}',
 '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"battle_report","arguments":{}}}'
)
$msgs | & target\debug\imperium-mcp.exe
```

## Pendiente

- **A\* a escala**: hoy se corre A* por unidad por tick (target dentro de VISION).
  Para miles hay que throttlear/cachear el path o usar flow-fields.
- `bevy_ecs_tilemap` para tiles texturizados — diferido a cuando haya arte (necesita
  atlas; el grid de mallas coloreadas alcanza por ahora).
- Órdenes restantes (retreat/unleash), tipos ranged (skirmishers).
- Spatial index linked-list sobre arrays (el `HashMap` actual es el placeholder; el
  cambio importa al empujar a miles).
- IA enemiga (behavior tree), BRP/MCP para manejar el juego desde agentes; Steamworks.

## Notas de diseño

- `sim_core` depende **solo** de `bevy_ecs` → corre sin ventana ni render. El test
  `battle_resolves_to_a_decided_outcome` construye un `World`, corre 500 ticks y
  assertea — el equivalente Rust del harness `sim-formations.ts`.
- Determinismo: el `Schedule` corre los sistemas con `.chain()` (orden secuencial).
- Las entidades comparten componentes de sim (`Hex`, `Health`, `Team`) y de render
  (`Mesh2d`, ...). Cuando el sim hace `despawn`, el sprite desaparece solo.
