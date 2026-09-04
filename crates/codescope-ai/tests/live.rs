//! Live provider smoke test. Ignored by default; opt in with:
//!
//! ```sh
//! CODESCOPE_LIVE=1 cargo test -p codescope-ai --test live -- --ignored
//! ```
//!
//! Uses [`AiConfig::from_env`]: set `PRIME_API_KEY`, `OPENAI_API_KEY`, or
//! `ANTHROPIC_API_KEY` (and optionally `CODESCOPE_AI_BASE_URL`).

use codescope_ai::{AiConfig, AiOutcome, AiService, FactView, Lookup, NoToolExecutor};
use codescope_core::{
    DiffSide, EntityRef, Epoch, FileId, LineRange, PlanEdgeKind, MAX_NODE_CODE_REFS,
};

/// Accept-everything facts: the live smoke exercises the wire + plan contract, not the
/// fact store (offline tests cover validation strictness).
struct AcceptAll;

impl FactView for AcceptAll {
    fn file(&self, _file: &FileId) -> Lookup<()> {
        Lookup::Present(())
    }
    fn symbol(&self, _file: &FileId, _name: &str) -> Lookup<LineRange> {
        Lookup::Present(LineRange::new(0, 0, 100_000, 0))
    }
    fn edge(&self, _from: &EntityRef, _to: &EntityRef, _kind: PlanEdgeKind) -> Lookup<()> {
        Lookup::Present(())
    }
    fn hunk(&self, _file: &FileId, _index: u32) -> Lookup<()> {
        Lookup::Present(())
    }
    fn diff_line(&self, _file: &FileId, _index: u32, _side: DiffSide, _line: u32) -> Lookup<()> {
        Lookup::Present(())
    }
    fn changed_diff_line(
        &self,
        _file: &FileId,
        _index: u32,
        _side: DiffSide,
        _line: u32,
    ) -> Lookup<()> {
        Lookup::Present(())
    }
}

const DIGEST: &str = "\
# changed symbols (1)
- internal/api/server.go: HandleRequest (function, modified) — added request logging and a nil-guard

# diagnostics (0)

# hunks (1)
- internal/api/server.go hunk 0: +5 -2 in HandleRequest

# 1-hop relationships
- HandleRequest is called by main (cmd/server/main.go: main)

## focused source evidence (exact selected hunks; hunk ids are zero-based; body annotations use one-based old/new lines)
hunk_id: 0  file: internal/api/server.go  @@ -40,5 +40,8 @@ func HandleRequest
[old:40 new:40]  func HandleRequest(req *Request) (*Response, error) {
[old:41 new:-] -	if req == nil {
[old:42 new:-] -		return nil, ErrBadRequest
[old:- new:41] +	req = normalize(req)
[old:- new:42] +	if req == nil {
[old:- new:43] +		return nil, ErrBadRequest
[old:- new:44] +	}
[old:- new:45] +	log.Printf(\"request id=%s\", req.ID)
[old:43 new:46] 	return handle(req)
[old:44 new:47] }
";

#[tokio::test]
#[ignore = "live provider smoke; set CODESCOPE_LIVE=1 and an API key to run"]
async fn live_plan_smoke() {
    if !codescope_testutil::live_ai_enabled() {
        eprintln!("SKIP: CODESCOPE_LIVE != 1");
        return;
    }
    let config = AiConfig::from_env().expect("ai config from env");
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
            // v4 strict rule: every validated node carries 1-2 exact code refs (a plan
            // without them is a parse error the repair loop must have fixed).
            for form in &plan.forms {
                for node in &form.nodes {
                    assert!(
                        (1..=MAX_NODE_CODE_REFS).contains(&node.code_refs.len()),
                        "live node {} lacks exact code_refs",
                        node.id
                    );
                }
            }
        }
        AiOutcome::Stale => panic!("model echoed a wrong epoch"),
        AiOutcome::Failed(reason) => panic!("live request failed: {reason}"),
        AiOutcome::Unavailable => panic!("provider unavailable"),
    }
}
