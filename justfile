set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Build and launch the Tauri desktop app (default).
default: app

# Build and launch the Tauri desktop app (dev mode — hot reload).
app:
    cd crates/app; pnpm tauri-dev

# List all available recipes.
list:
    @just --list

publish version="":
    uv -g commit-tag-push {{version}}

# ---------------- distribution ----------------
# Release-build the workspace, then assemble standalone packages in
# target/dist/{app,host,client}/.
#
# - app/    -> the Tauri desktop app (mush_stream.exe + frontend
#              embedded + ffmpeg DLLs). Single-exe distribution; runs
#              the host + client pipelines in-process.
# - host/   -> the standalone CLI host (legacy / scripted use).
# - client/ -> the standalone CLI client (legacy / scripted use).

# Send the whole subfolder to a friend.
[windows]
build:
    # Build the standalone CLI binaries (host, client, common). The
    # Tauri crate is excluded here because `tauri build` below owns
    # its build pipeline end-to-end (frontend bundling + resource
    # embedding + installer creation) and a plain `cargo build` of
    # the Tauri crate would skip the bundling step.
    cargo build --release -p mush-stream-host -p mush-stream-client

    # Build the Tauri desktop app. This runs `vite build` (per
    # `beforeBuildCommand`), then `cargo build --release` on the
    # Tauri crate with the frontend resources embedded, then
    # produces NSIS + MSI installers under target/release/bundle/.
    cd crates/app; pnpm install --frozen-lockfile; pnpm tauri-build

    # Layout the per-binary subfolders.
    New-Item -ItemType Directory -Force -Path target/dist/app/installers | Out-Null
    New-Item -ItemType Directory -Force -Path target/dist/host           | Out-Null
    New-Item -ItemType Directory -Force -Path target/dist/client         | Out-Null

    # Binaries.
    Copy-Item target/release/mush_stream.exe        target/dist/app/
    Copy-Item target/release/mush-stream-host.exe   target/dist/host/
    Copy-Item target/release/mush-stream-client.exe target/dist/client/

    # ffmpeg runtime DLLs (placed in target/release/ by the host and
    # client crates' build.rs from $env:FFMPEG_DIR/bin/*.dll). The
    # Tauri app embeds both host + client libraries so it needs the
    # same DLLs alongside its exe.
    Copy-Item target/release/*.dll target/dist/app/
    Copy-Item target/release/*.dll target/dist/host/
    Copy-Item target/release/*.dll target/dist/client/

    # Tauri's bundle output: NSIS .exe installer + MSI installer.
    # Flatten both formats into target/dist/app/installers/ so the
    # entire shippable app lives under target/dist/app/.
    if (Test-Path target/release/bundle/nsis) { Copy-Item target/release/bundle/nsis/*.exe target/dist/ }
    if (Test-Path target/release/bundle/msi)  { Copy-Item target/release/bundle/msi/*.msi  target/dist/ }

    # Example configs and the README. The Tauri app writes its own
    # configs into the per-user app config dir on first run, so it
    # only ships the README — no .toml seed needed there.
    Copy-Item README.md           target/dist/app/
    Copy-Item host.toml.example   target/dist/host/host.toml
    Copy-Item client.toml.example target/dist/client/client.toml
    Copy-Item README.md           target/dist/host/
    Copy-Item README.md           target/dist/client/

    Write-Host ""
    Write-Host "dist ready:"
    Write-Host "  target/dist/app/             -> mush_stream.exe + DLLs (portable single-exe app)"
    Write-Host "  target/dist/app/installers/  -> NSIS + MSI installers"
    Write-Host "  target/dist/host/            -> mush-stream-host.exe + DLLs + host.toml.example"
    Write-Host "  target/dist/client/          -> mush-stream-client.exe + DLLs + client.toml.example"

# ---------------- dev tasks ----------------

# Debug build of the whole workspace (no dist packaging).
debug:
    cargo build --workspace

# Run the host binary (debug). Pass extra args after `--`.
run-host *ARGS:
    cargo run -p mush-stream-host -- {{ ARGS }}

# Run the client binary (debug). Pass extra args after `--`.
run-client *ARGS:
    cargo run -p mush-stream-client -- {{ ARGS }}

# Run all unit tests.
test:
    cargo test --workspace

# clippy --pedantic --all-targets, treating warnings as errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Drop cargo target/ entirely (including dist/).
clean:
    cargo clean
