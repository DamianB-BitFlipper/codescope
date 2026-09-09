# Publishing to crates.io

Codescope's public binary depends on the other crates in this workspace. Local development uses
path dependencies; packaged manifests use the matching exact crates.io version. Publish every
crate at the same version and wait for each dependency layer to appear in the crates.io index
before publishing the next layer.

## Prepare the release

Start from a clean checkout and run:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
git diff --check
cargo publish --locked --workspace --dry-run
```

The workspace dry run validates every archive and verifies dependent crates against Cargo's
temporary local registry without uploading anything.

## Publish

Use the resumable publishing script:

```bash
./scripts/publish.sh
```

It publishes one crate at a time in dependency order, skips versions already present on crates.io,
and automatically waits until the server-provided retry time after an HTTP 429 response. This
avoids leaving a rate-limited workspace release that cannot be resumed with
`cargo publish --locked --workspace`, because Cargo stops when it reaches a crate uploaded by the
previous attempt.

The script requires typing `publish` before the first upload. For an intentional non-interactive
release, use `./scripts/publish.sh --yes`.

Before a later release, update `workspace.package.version` and every version in
`workspace.dependencies` together, then regenerate `Cargo.lock`.
