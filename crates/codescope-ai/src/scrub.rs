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
            // Common standalone provider token shapes. Boundaries avoid matching
            // ordinary words which merely contain a prefix such as `sk` or `pk`.
            (r#"(?i)\b(?:sk-(?:ant-)?|pk-|ghp_|gho_|ghu_|ghs_|ghr_|github_pat_|xox[baprs]-|AKIA|ASIA)[A-Za-z0-9_-]{12,}\b"#, 0),
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

/// Preserve a provider payload's JSON shape while scrubbing every string value.
#[must_use]
pub(crate) fn scrub_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(scrub_secrets(text)),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(scrub_json).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), scrub_json(value)))
                .collect(),
        ),
        other => other.clone(),
    }
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
    fn scrubs_standalone_provider_tokens() {
        let tokens = [
            "sk-abcdef012345",
            "sk-ant-abcdef012345",
            "pk-abcdef012345",
            "ghp_abcdef012345",
            "gho_abcdef012345",
            "ghu_abcdef012345",
            "ghs_abcdef012345",
            "ghr_abcdef012345",
            "github_pat_abcdef012345",
            "xoxb-abcdef012345",
            "xoxa-abcdef012345",
            "xoxp-abcdef012345",
            "xoxr-abcdef012345",
            "xoxs-abcdef012345",
            "AKIAabcdef012345",
            "ASIAabcdef012345",
        ];

        for token in tokens {
            let out = scrub_secrets(token);
            assert!(!out.contains(token), "token leaked: {token}");
            assert!(out.contains(REDACTED), "token was not redacted: {token}");
        }
    }

    #[test]
    fn scrubs_provider_tokens_surrounded_by_punctuation_and_newlines() {
        let token = "sk-abcdef012345";
        let out = scrub_secrets(&format!("before (\n{token}\n), after"));
        assert!(!out.contains(token));
        assert!(out.contains(REDACTED));
        assert!(out.contains("before (\n"));
        assert!(out.contains("\n), after"));
    }

    #[test]
    fn leaves_provider_prefixes_inside_words() {
        let text = "whiskeysk-abcdef012345 desktoppk-abcdef012345 xghp_abcdef012345";
        assert_eq!(scrub_secrets(text), text);
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

    #[test]
    fn structured_scrubbing_preserves_provider_payload_shape() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "token sk-abcdef012345"}],
            "stream": false
        });
        let scrubbed = scrub_json(&body);
        assert_eq!(scrubbed["messages"][0]["role"], "user");
        assert_eq!(scrubbed["stream"], false);
        assert_eq!(
            scrubbed["messages"][0]["content"],
            "token [redacted-secret]"
        );
    }
}
