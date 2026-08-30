//! Live provider smoke test. Ignored by default; opt in with:
//!
//! ```sh
//! CODESCOPE_LIVE=1 cargo test -p codescope-ai --test live -- --ignored
//! ```
//!
//! Uses [`AiConfig::from_env`]: set `CODESCOPE_AI_API_KEY` / `PRIME_API_KEY` /
//! `OPENAI_API_KEY` (and optionally `CODESCOPE_AI_BASE_URL`, `CODESCOPE_AI_MODEL`).

use codescope_ai::{AiConfig, AiOutcome, AiService, FactView, NoToolExecutor};
use codescope_core::{EntityRef, Epoch, FileId, LineRange, PlanEdgeKind};

/// Accept-everything facts: the live smoke exercises the wire + plan contract, not the
/// fact store (offline tests cover validation strictness).
struct AcceptAll;

impl FactView for AcceptAll {
    fn file_exists(&self, _file: &FileId) -> bool {
        true
    }
    fn resolve_symbol(&self, _file: &FileId, _name: &str) -> Option<LineRange> {
        Some(LineRange::new(0, 0, 100_000, 0))
    }
    fn edge_exists(&self, _from: &EntityRef, _to: &EntityRef, _kind: PlanEdgeKind) -> bool {
        true
    }
    fn hunk(&self, _file: &FileId, _index: u32) -> Option<()> {
        Some(())
    }
}

const DIGEST: &str = "\
# changed symbols (1)
- internal/api/server.go: HandleRequest (function, modified) — added request logging and a nil-guard

# diagnostics (0)

# hunks (1)
- internal/api/server.go hunk 0: +6 -1 in HandleRequest

# 1-hop relationships
- HandleRequest is called by main (cmd/server/main.go: main)
";

#[tokio::test]
#[ignore = "live provider smoke; set CODESCOPE_LIVE=1 and an API key to run"]
async fn live_plan_smoke() {
    if !codescope_testutil::live_ai_enabled() {
        eprintln!("SKIP: CODESCOPE_LIVE != 1");
        return;
    }
    let config = AiConfig::from_env().expect("ai config from env");
    if !config.enabled {
        eprintln!("SKIP: no API key found in env");
        return;
    }
    let service = AiService::new(config, "/tmp/codescope-live-smoke").expect("service");
    let outcome = service
        .request_plan(DIGEST, &NoToolExecutor, &AcceptAll, Epoch(1))
        .await;
    match outcome {
        AiOutcome::Plan(plan, report) => {
            eprintln!("live plan: {plan:#?}\nreport: {report:#?}");
            assert!(report.is_renderable());
            assert!(!plan.forms.is_empty());
            assert_eq!(plan.epoch, Epoch(1), "model must echo the prompt epoch");
        }
        AiOutcome::Stale => panic!("model echoed a wrong epoch"),
        AiOutcome::Failed(reason) => panic!("live request failed: {reason}"),
        AiOutcome::Unavailable => panic!("provider unavailable"),
    }
}
