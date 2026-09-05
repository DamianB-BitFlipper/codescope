# Publishing to crates.io

Codescope's public binary depends on the other crates in this workspace. Local development uses
path dependencies; packaged manifests use the matching exact crates.io version. Publish every
crate at the same version and wait for each dependency layer to appear in the crates.io index
before publishing the next layer.

## Prepare the release

Start from a clean checkout and run:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
cargo package --workspace --no-verify
```

`cargo package --workspace --no-verify` validates every archive and normalized manifest together.
Full package verification of a dependent crate becomes possible only after that crate's internal
dependencies have been published.

## Publish in dependency order

For `0.1.0-alpha.1`, publish in these layers:

```bash
cargo publish -p codescope-core
cargo publish -p codescope-telemetry

cargo publish -p codescope-git
cargo publish -p codescope-lsp
cargo publish -p codescope-testutil

cargo publish -p codescope-ai
cargo publish -p codescope-analysis
cargo publish -p codescope-tui

cargo publish -p codescope
```

Wait for crates.io index propagation between each blank-line-separated layer. Before a later
release, update `workspace.package.version` and every version in `workspace.dependencies`
together, then regenerate `Cargo.lock`.
