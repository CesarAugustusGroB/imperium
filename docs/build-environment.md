# Build environment — unblocking `cargo` in the sandboxed web/CI sessions

This note exists because automated agent runs (Claude Code on the web, and any
similarly-sandboxed CI) keep hitting the **same hard wall**: they cannot
download crates, so `cargo test -p sim_core` — the project's entire
verification loop — never runs. Without it, a run can only either push
**untested** simulation code (forbidden by the project's hard rule that every
change keep `cargo test -p sim_core` green) or produce yet another
"could-not-build" proposal. Fixing the environment once unblocks all future
runs. This is that fix.

## Symptom

```
$ cargo test -p sim_core
warning: spurious network error (… tries remaining): [56] CONNECT tunnel failed, response 403
…
error: failed to download from `https://static.crates.io/crates/arrayvec/0.7.6/download`
Caused by:
  [56] Failure when receiving data from the peer (CONNECT tunnel failed, response 403)
```

## Root cause (measured, not guessed)

The sandbox routes outbound HTTPS through a policy-enforcing egress proxy. With
Cargo's **sparse registry** protocol the index and the tarballs live on two
different hosts, and the policy allows one but not the other:

| Host | Purpose | Reachable from sandbox |
|------|---------|------------------------|
| `index.crates.io` | sparse **index** (metadata: versions, checksums) | ✅ `200` |
| `static.crates.io` | crate **tarballs** (the actual `.crate` downloads) | ❌ `403` CONNECT denied |
| `github.com` | git deps / source | ✅ `200` |

So `cargo` resolves the dependency graph fine (it can read the index) and then
fails the instant it tries to *download* a crate. The local registry cache
(`~/.cargo/registry/{cache,src}`) is empty and there is no committed `vendor/`
directory, so there is no offline fallback. This is an **organization egress
policy denial** (HTTP 403 at the proxy), not a TLS/CA problem — `CARGO_HTTP_CAINFO`
is set correctly and `index.crates.io` negotiates TLS fine. It therefore cannot
be worked around from inside a session; it has to be fixed where the environment
is configured.

Confirm the denial yourself from inside a session:

```sh
curl -sS "$HTTPS_PROXY/__agentproxy/status" | grep -A2 static.crates.io   # shows the 403 connect_rejected entries
curl -o /dev/null -w '%{http_code}\n' https://static.crates.io/           # 000 (CONNECT 403)
curl -o /dev/null -w '%{http_code}\n' https://index.crates.io/            # 200
```

## Fixes, best first

### 1. Allowlist `static.crates.io` in the environment's network policy  ← recommended

The cleanest fix: add `static.crates.io` (keep `index.crates.io`) to the
allowed-egress list for the web/CI environment. One line, and every future run
builds normally with no repo changes. For Claude Code on the web this is the
environment's **network policy** — see
<https://code.claude.com/docs/en/claude-code-on-the-web>. The minimal allowlist
for this workspace is:

```
index.crates.io      # sparse index (already allowed)
static.crates.io     # crate tarballs  ← ADD THIS
github.com           # already allowed
```

### 2. Pre-warm the registry cache in the environment setup script

If the egress policy can't change but the setup script runs **once with
network** before the policy clamps down, have it populate the cache so later
offline steps hit it:

```sh
cargo fetch --locked    # downloads every crate in Cargo.lock into ~/.cargo/registry
```

After this, `cargo test -p sim_core --offline` works for the rest of the session.

### 3. Vendor dependencies (no policy change needed)

If neither of the above is possible, vendor the locked dependency set from a
**networked machine** and commit the result. Thereafter the sandbox builds with
zero crates.io access. Use the helper:

```sh
./scripts/vendor-deps.sh          # run on a machine WITH crates.io access
```

It runs `cargo vendor` against the committed `Cargo.lock` and writes the source
replacement config. Trade-off: the `vendor/` tree for the full Bevy graph is
large (hundreds of MB across ~650 crates), so most teams prefer fix #1 or #2.
Keep `vendor/` out of git unless you specifically want fully-offline clones; the
script prints the `.cargo/config.toml` stanza either way.

## Why this run shipped docs, not simulation code

`sim_core`'s contract is that **every** change is proven by `cargo test -p sim_core`.
This sandbox cannot run that command (above), and there is no cached or vendored
dependency set to fall back to, so any simulation change would be **unverified** —
exactly what the project forbids. Rather than push untested ECS logic or repeat a
prior run's design proposal, this run delivers the thing that actually unblocks
the verification loop for everyone after it. Apply fix #1 (or #2) and subsequent
runs can return to shipping tested engine improvements.
