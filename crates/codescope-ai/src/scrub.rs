//! Outbound content scrubbing: defense-in-depth against sending secrets to the provider.
//!
//! Layer 4 of the privacy model (research 07): even after path redaction, a *tracked* secret
//! file or an inline key can appear inside a hunk preview. This pass scrubs recognizable
//! secret shapes from any text before it leaves the machine. It is intentionally conservative:
//! it replaces the secret value, never the surrounding structure, so the digest stays useful.

use std::sync::OnceLock;

/// Replacement marker for scrubbed secret material.
pub const REDACTED: &str = "[redacted-secret]";

/// A compiled secret pattern: `(regex, keep_prefix_len)`.
struct Rule {
    re: regex::Regex,
    /// Characters of the match to keep (so `api_key=` survives, the value does not).
    keep_prefix: usize,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let specs: &[(&str, usize)] = &[
            // key=value assignments with a secret-looking key
            (r#"(?i)(api[_-]?key|api[_-]?secret|secret[_-]?key|access[_-]?token|auth[_-]?token|private[_-]?key|client[_-]?secret|password|passwd|aws[_-]?secret)\s*[:=]\s*["']?[A-Za-z0-9/_+.-]{8,}"#, 0),
            // JWTs
            (r#"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"#, 0),
            // bearer tokens
            (r#"(?i)bearer\s+[A-Za-z0-9/_+.-]{16,}"#, 7),
            // PEM blocks
            (r#"-----BEGIN [A-Z ]*PRIVATE KEY-----"#, 0),
            // common provider key shapes (sk-…, ghp_…, xox…, AKIA…)
            (r#"(sk|pk|ghp|gho|ghu|ghs|ghr|xox[baprs]|AKIA|ASIA)[A-Za-z0-9_-]{16,}"#, 0),
        ];
        specs
            .iter()
            .map(|(pat, keep)| Rule {
                re: regex::Regex::new(pat).expect("valid secret regex"),
                keep_prefix: *keep,
            })
            .collect()
    })
}

/// Scrub recognizable secret material from `text`, replacing values with [`REDACTED`].
#[must_use]
pub fn scrub_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for rule in rules() {
        out = rule
            .re
            .replace_all(&out, |caps: &regex::Captures<'_>| {
                let m = &caps[0];
                let keep = rule.keep_prefix.min(m.len());
                format!("{}{}", &m[..keep], REDACTED)
            })
            .into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_key_assignments() {
        let s = "OPENAI_API_KEY=sk-abcdef0123456789abcdef\nother=ok";
        let out = scrub_secrets(s);
        assert!(!out.contains("abcdef0123456789"));
        assert!(out.contains(REDACTED));
        assert!(out.contains("other=ok"));
    }

    #[test]
    fn scrubs_jwt_and_bearer() {
        assert!(!scrub_secrets("Authorization: Bearer abcdef0123456789xyz")
            .contains("abcdef0123456789xyz"));
        assert!(scrub_secrets("token eyJhbGciOiJ9.eyJzdWIiOiIxIn0.abcDEF123").contains(REDACTED));
    }

    #[test]
    fn scrubs_pem_header() {
        assert!(scrub_secrets("-----BEGIN PRIVATE KEY-----\nMIIB...").contains(REDACTED));
    }

    #[test]
    fn leaves_normal_code() {
        let code = "func (r *MemoryRepo) Get(id int64) (*User, error) { return r.m[id], nil }";
        assert_eq!(scrub_secrets(code), code);
    }

    #[test]
    fn leaves_short_values() {
        let s = "api_key = dev"; // too short to look like a real key
        assert_eq!(scrub_secrets(s), s);
    }
}
