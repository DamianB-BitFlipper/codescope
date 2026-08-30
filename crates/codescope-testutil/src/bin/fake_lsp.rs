//! `fake-lsp` — the scriptable fake LSP server on real stdio.
//!
//! Usage: `fake_lsp [script.json]` where `script.json` is a serialized
//! [`FakeLspConfig`]. Without an argument it
//! serves the gopls-like default config. Lets LSP-client tests exercise a real subprocess
//! (spawn/kill/shutdown paths) instead of an in-process duplex pipe.

use anyhow::Context;
use codescope_testutil::fake_lsp::{FakeLspConfig, FakeLspServer};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = match std::env::args().nth(1) {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading fake-lsp script {path}"))?;
            serde_json::from_str::<FakeLspConfig>(&raw)
                .with_context(|| format!("parsing fake-lsp script {path}"))?
        }
        None => FakeLspConfig::default(),
    };
    let server = FakeLspServer::new(config);
    server
        .serve_stdio()
        .await
        .context("fake-lsp session failed")?;
    Ok(())
}
