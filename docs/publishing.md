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

## Publish

The publishing script releases each crate in dependency order and waits for crates.io index
propagation before continuing:

```bash
./scripts/publish.sh
```

It requires typing `publish` before the first upload. Use `./scripts/publish.sh --yes` only for an
intentional non-interactive release. The script is resumable: packages already published at the
workspace version are skipped.

Before a later release, update `workspace.package.version` and every version in
`workspace.dependencies` together, then regenerate `Cargo.lock`.
