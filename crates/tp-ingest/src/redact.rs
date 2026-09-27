//! Secret scrubbing, applied before anything is written. `thinking` is
//! scrubbed alongside `text` because a model tends to restate a secret
//! verbatim while reasoning about it.

use once_cell::sync::Lazy;
use regex::Regex;
use tp_core::turn::NormalizedTurn;

struct RedactRule {
    pattern: Regex,
    replacement: &'static str,
}

static RULES: Lazy<Vec<RedactRule>> = Lazy::new(|| {
    let specs: &[(&str, &str)] = &[
        (r"AKIA[0-9A-Z]{16}", "[redacted:aws-access-key]"),
        (r"sk-ant-[A-Za-z0-9_-]{20,}", "[redacted:anthropic-key]"),
        (r"sk-[A-Za-z0-9]{20,}", "[redacted:api-key]"),
        (r"gh[pousr]_[A-Za-z0-9]{36,}", "[redacted:github-token]"),
        (
            r"(?i)bearer\s+[A-Za-z0-9\-_.]{20,}",
            "[redacted:bearer-token]",
        ),
        (
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "[redacted:private-key]",
        ),
        // OAuth token pairs as they appear in dumped keychain / credential JSON.
        (
            r#""accessToken"\s*:\s*"[^"]{10,}""#,
            "\"accessToken\":\"[redacted]\"",
        ),
        (
            r#""refreshToken"\s*:\s*"[^"]{10,}""#,
            "\"refreshToken\":\"[redacted]\"",
        ),
        // Stripe keys use an underscore separator, so the hyphenated `sk-` rule
        // above does not reach them. Restricted keys (rk_) share the shape.
        (
            r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
            "[redacted:stripe-key]",
        ),
        (
            r"\bwhsec_[A-Za-z0-9]{16,}",
            "[redacted:stripe-webhook-secret]",
        ),
        // Connection strings: only the password is replaced. Scheme, user and
        // host are what make a transcript searchable, and none is the secret.
        // Requires `:pass@` so a credential-less `redis://host:6379` is left alone.
        (
            r"(?i)\b(postgres|postgresql|mysql|mongodb\+srv|mongodb|rediss|redis|amqps|amqp)://([^:@/\s]+):[^@/\s]+@",
            "${1}://${2}:[redacted]@",
        ),
        // JWTs: three base64url segments. `eyJ` is base64 for `{"`, so the
        // prefix identifies a JSON header rather than matching by coincidence.
        (
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            "[redacted:jwt]",
        ),
        (r"\bAIza[0-9A-Za-z_-]{35}", "[redacted:google-api-key]"),
        (r"\bxox[baprs]-[A-Za-z0-9-]{10,}", "[redacted:slack-token]"),
        (r"\bnpm_[A-Za-z0-9]{36}", "[redacted:npm-token]"),
        (r"\bgithub_pat_[A-Za-z0-9_]{22,}", "[redacted:github-pat]"),
    ];
    specs
        .iter()
        .map(|(p, r)| RedactRule {
            pattern: Regex::new(p).expect("static redact pattern"),
            replacement: r,
        })
        .collect()
});

/// Public because the retrieval funnel applies it to every provider's output,
/// not only to turns on the ingest path. Must be idempotent: a stored value is
/// scrubbed again on read, so no rule may match its own replacement text.
pub fn scrub(s: &str) -> String {
    let mut out = s.to_string();
    for rule in RULES.iter() {
        if rule.pattern.is_match(&out) {
            out = rule
                .pattern
                .replace_all(&out, rule.replacement)
                .into_owned();
        }
    }
    out
}

pub fn redact(turn: &mut NormalizedTurn) {
    turn.text = scrub(&turn.text);
    turn.thinking = scrub(&turn.thinking);
    for tc in &mut turn.tool_calls {
        if let Some(d) = &tc.input_digest {
            tc.input_digest = Some(scrub(d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_core::turn::Role;

    fn turn(text: &str, thinking: &str) -> NormalizedTurn {
        NormalizedTurn {
            role: Role::Assistant,
            ts: None,
            text: text.to_string(),
            thinking: thinking.to_string(),
            thinking_opaque: false,
            tool_calls: vec![],
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Default::default(),
        }
    }

    #[test]
    fn redacts_anthropic_key_in_thinking() {
        let mut t = turn(
            "",
            "the key is sk-ant-oat01-abcdefghijklmnopqrstuvwxyz123456",
        );
        redact(&mut t);
        assert!(!t.thinking.contains("sk-ant-oat01"));
        assert!(t.thinking.contains("[redacted:anthropic-key]"));
    }

    #[test]
    fn redacts_bearer_token() {
        let mut t = turn(
            "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc.def'",
            "",
        );
        redact(&mut t);
        assert!(!t.text.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let mut t = turn(
            "just a normal sentence about the design",
            "reasoning about session ids",
        );
        let before = (t.text.clone(), t.thinking.clone());
        redact(&mut t);
        assert_eq!((t.text, t.thinking), before);
    }
    // Shapes the generic rules do not reach: Stripe's underscore separator is
    // outside the hyphenated `sk-` rule.

    #[test]
    fn redacts_stripe_secret_key_underscore_form() {
        let mut t = turn("STRIPE_SECRET_KEY=sk_test_51QxAbCdEfGhIjKlMnOpQrStUv", "");
        redact(&mut t);
        assert!(!t.text.contains("51QxAbCdEfGhIjKlMnOpQrStUv"));
        assert!(t.text.contains("[redacted:stripe-key]"));
    }

    #[test]
    fn redacts_stripe_webhook_secret() {
        let mut t = turn("whsec_AbCdEfGhIjKlMnOpQrStUvWx1234", "");
        redact(&mut t);
        assert!(t.text.contains("[redacted:stripe-webhook-secret]"));
    }

    #[test]
    fn redacts_connection_string_password_but_keeps_the_rest() {
        let mut t = turn("postgres://appuser:hunter2@192.0.2.10:5432/appdb", "");
        redact(&mut t);
        assert!(!t.text.contains("hunter2"));
        // scheme, user, host and database survive — they are what makes the
        // transcript worth searching, and none of them is the secret.
        assert!(t
            .text
            .contains("postgres://appuser:[redacted]@192.0.2.10:5432/appdb"));
    }

    #[test]
    fn leaves_credential_less_connection_string_alone() {
        let mut t = turn("redis://192.0.2.1:6379", "");
        redact(&mut t);
        assert_eq!(t.text, "redis://192.0.2.1:6379");
    }

    #[test]
    fn redacts_jwt() {
        let mut t = turn(
            "anon key: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJyb2xlIjoiYW5vbiJ9.YZW-GDIN7q6372HaRCr",
            "",
        );
        redact(&mut t);
        assert!(!t.text.contains("eyJyb2xlIjoiYW5vbiJ9"));
        assert!(t.text.contains("[redacted:jwt]"));
    }

    #[test]
    fn redacts_google_slack_npm_and_github_pat() {
        let mut t = turn(
            "AIzaSyD-1234567890abcdefghijklmnopqrstu \
             xoxb-123456789012-1234567890123-AbCdEfGhIj \
             npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 \
             github_pat_11ABCDEFG0abcdefghijklmnop",
            "",
        );
        redact(&mut t);
        for leaked in [
            "AIzaSyD-1234567890abcdefghijklmnopqrstu",
            "xoxb-123456789012",
            "npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "github_pat_11ABCDEFG0abcdefghijklmnop",
        ] {
            assert!(!t.text.contains(leaked), "leaked: {leaked}");
        }
    }

    #[test]
    fn scrubbing_is_idempotent() {
        // A stored value is scrubbed on retrieval as well as ingest; a rule
        // that matched its own replacement text would corrupt it.
        let once = scrub("postgres://u:p@h/db sk_live_AbCdEfGhIjKlMnOpQr");
        assert_eq!(once, scrub(&once));
    }
}
