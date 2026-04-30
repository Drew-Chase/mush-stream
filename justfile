set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# List recipes when run with no args.
default:
    @just --list

# ---------------- distribution ----------------
# Release-build the workspace, then assemble standalone host + client
# packages in target/dist/{host,client}/. Each subfolder gets the
# binary, the ffmpeg runtime DLLs (auto-copied next to the binary by
# each crate's build.rs), the matching .toml.example, and the README.

# Send the whole subfolder to a friend.
[windows]
build:
    cargo build --workspace --release

    # Wipe any previous dist/ output.
    # if (Test-Path target/dist) { Remove-Item -Recurse -Force target/dist }

    # Layout the per-binary subfolders.
    New-Item -ItemType Directory -Force -Path target/dist/host   | Out-Null
    New-Item -ItemType Directory -Force -Path target/dist/client | Out-Null

    # Binaries.
    Copy-Item target/release/mush-stream-host.exe   target/dist/host/
    Copy-Item target/release/mush-stream-client.exe target/dist/client/

    # ffmpeg runtime DLLs (placed in target/release/ by each crate's
    # build.rs from $env:FFMPEG_DIR/bin/*.dll).
    Copy-Item target/release/*.dll target/dist/host/
    Copy-Item target/release/*.dll target/dist/client/

    # Example configs and the README, so the friend has something to
    # rename to host.toml / client.toml and read.
    Copy-Item host.toml.example   target/dist/host/host.toml
    Copy-Item client.toml.example target/dist/client/client.toml
    Copy-Item README.md           target/dist/host/
    Copy-Item README.md           target/dist/client/

    Write-Host ""
    Write-Host "dist ready:"
    Write-Host "  target/dist/host/    -> mush-stream-host.exe + DLLs + host.toml.example"
    Write-Host "  target/dist/client/  -> mush-stream-client.exe + DLLs + client.toml.example"

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
