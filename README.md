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
  avanza para cerrar, *retreat* cuando la barren (< 0.5×), *hold* cuando va
  perdiendo, *charge* cuando va ganando y hay contacto, y *unleash* (ataque total)
  cuando domina (≥ 1.25×) y ya está en contacto. Emula el estilo "amasar → lanzar
  / defender perdiendo".
- **BRP** (Bevy Remote Protocol): el ECS se expone por JSON-RPC en el puerto 15702;
  un agente puede leer/mutar el juego corriendo (ver sección *Agentes*).
- **Terreno** con efecto mecánico: montaña/agua intransitables, bosque/colina
  ralentizan (mayor cooldown) y dan **bonus defensivo** (menos daño recibido).
  Las **montañas bloquean la línea de visión**: un skirmisher no puede disparar a
  través de una montaña — apunta al enemigo *visible* más cercano (los extremos no
  cuentan, así que un objetivo parado sobre una montaña sigue siendo disparable).
  Generación determinista por semilla (hash noise, sin deps).
- **Pathfinding y formaciones**: A\* (hexx) rutea alrededor de montañas/agua hacia
  el enemigo visible, pero el path se **cachea y se recomputa sólo cada N ticks**
  (no por unidad por tick) para escalar. Sin enemigo a la vista el ejército sigue
  un **flow-field** (campo de integración Dijkstra compartido hacia la línea
  enemiga, rodea obstáculos cóncavos que un paso greedy no resuelve). Hay
  **evasión de bloqueo** (un `sidestep` lateral cuando una unidad queda encajonada
  por sus propias filas) y **cohesión de formación** (una unidad que se adelanta
  más de `COHESION_SLACK` del frente de su grupo se frena para que el bloque cierre).
- **Escala**: índice espacial **denso** (array sobre la caja envolvente, en vez del
  `HashMap` placeholder) y un escaneo de enemigo cercano **anillo por anillo** con
  salida temprana. Hay tests de estrés headless (cientos/miles de unidades) que
  verifican que el tick no panica y el conteo sólo decrece.
- **Órdenes por grupo** (March / Charge / Hold / Idle / **Retreat** / **Unleash**)
  y **cooldowns** por tipo: charge pega más y va más rápido, hold reduce daño,
  **retreat** se repliega alejándose del enemigo hacia la línea propia, y
  **unleash** es el ataque total (paso de carga, el mayor bonus de daño y los
  skirmishers dejan de *kitear* para entrar al cuerpo a cuerpo).
- Controles: **`1`** March · **`2`** Charge · **`3`** Hold · **`4`** Retreat ·
  **`5`** Unleash (todos para Red, grupo 1).
- **Combate**: cada unidad lanza **un** ataque por tick. En melee enfoca al enemigo
  adyacente más débil (*focus fire*, asegura bajas) y el golpe se **amplifica por
  flanqueo** (más atacantes rodeando al mismo objetivo → más daño; rodear es letal).
  Los skirmishers disparan a distancia (sin bonus de flanqueo).
- **Stamina / fatiga**: la agresión sostenida cuesta. Cargar/*unleash* drena stamina;
  *hold*/idle la recupera. Una unidad *winded* (stamina baja) pierde su **bonus de
  carga** — una carga larga deja de rendir y conviene rotar tropas frescas. No toca
  el daño base.
- **Capa de animación (datos)**: cada unidad lleva un `AnimState`
  (idle/move/attack/hit/die) recomputado por tick; el sim emite `AttackEvent` /
  `DeathEvent` en `BattleEvents` y un `AnimCatalog` tipado mapea `(Kind, AnimState)`
  a frames — el arte queda para el humano, el esquema es el contrato.
- **Determinismo**: el tick es bit-a-bit reproducible (test de propiedad), la
  matemática de balance de la IA no desborda a escala de millones, y casos límite
  (mundo vacío, unidad totalmente amurallada) tickean sin panic.
- Toda la lógica vive en `sim_core` (ECS puro sobre `bevy_ecs`, **headless, testeable**:
  59 tests).
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

> ⚠️ **Sandbox sin acceso a crates.io** (agentes web / CI con egress restringido):
> si `cargo` falla con `static.crates.io … CONNECT tunnel failed, response 403`,
> el entorno bloquea la descarga de crates y ningún `cargo test` puede correr.
> El diagnóstico y los arreglos (allowlist `static.crates.io`, pre-warm del
> registry, o vendoring offline con `scripts/vendor-deps.sh`) están en
> [`docs/build-environment.md`](docs/build-environment.md).

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

- **Render de animaciones**: la *capa de datos* ya está (`AnimState` + eventos +
  `AnimCatalog`); falta que el crate `imperium` consuma el catálogo y dibuje los
  sprite-sheets (hoy las paths del catálogo son placeholders, falta el arte).
- `bevy_ecs_tilemap` para tiles texturizados — diferido a cuando haya arte (necesita
  atlas; el grid de mallas coloreadas alcanza por ahora).
- IA enemiga más rica (behavior tree, órdenes por grupo en vez de army-level);
  Steamworks.

## Notas de diseño

- `sim_core` depende **solo** de `bevy_ecs` → corre sin ventana ni render. El test
  `battle_resolves_to_a_decided_outcome` construye un `World`, corre 500 ticks y
  assertea — el equivalente Rust del harness `sim-formations.ts`.
- Determinismo: el `Schedule` corre los sistemas con `.chain()` (orden secuencial).
- Las entidades comparten componentes de sim (`Hex`, `Health`, `Team`) y de render
  (`Mesh2d`, ...). Cuando el sim hace `despawn`, el sprite desaparece solo.
