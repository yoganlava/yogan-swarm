const MASK: &str = "[REDACTED]";

/// Prefixes of known key formats; each needs at least 16 more characters to count.
const PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "npm_",
    "pypi-",
    "eyJ",
];

/// Key names (lowercased, `_` and `-` removed) whose assigned value is masked.
const SECRET_KEYS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "clientid",
    "apikey",
    "privatekey",
];

/// Masks token-like strings. Only runs of `[A-Za-z0-9_+/-]` are replaced, so JSON stays valid.
pub fn redact(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut gap_start, mut prev) = (0, 0, "");
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2; // an escape such as \n or \" is never part of a token
            continue;
        }
        if !is_token(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_token(b[i]) {
            i += 1;
        }
        let (gap, run) = (&text[gap_start..start], &text[start..i]);
        out.push_str(gap);
        let secret = known_format(run) || (names_secret(prev) && assigns(gap)) || random(run);
        out.push_str(if secret { MASK } else { run });
        (prev, gap_start) = (run, i);
    }
    out.push_str(&text[gap_start.min(b.len())..]);
    out
}

fn is_token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+' | b'/')
}

fn known_format(run: &str) -> bool {
    PREFIXES
        .iter()
        .any(|p| run.starts_with(p) && run.len() >= p.len() + 16)
}

fn names_secret(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| k.contains(s)) || k.ends_with("token")
}

/// `KEY=value`, or a quoted value after `:` or `=` (JSON, TOML, YAML, escaped JSON).
fn assigns(gap: &str) -> bool {
    if gap == "=" {
        return true;
    }
    let g: String = gap
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\\')
        .collect();
    let g = g.strip_prefix(['"', '\'']).unwrap_or(&g);
    matches!(g, ":\"" | ":'" | "=\"" | "='")
}

// ponytail: character-class heuristic, not Shannon entropy. Misses all-hex and single-case
// secrets (those rely on PREFIXES); add a real entropy score if one slips through.
/// Long, mixes upper, lower and digits, and switches class like random text (~0.6), not words (~0.2).
fn random(run: &str) -> bool {
    let class = |c: u8| match c {
        b'A'..=b'Z' => 0,
        b'a'..=b'z' => 1,
        b'0'..=b'9' => 2,
        _ => 3,
    };
    let b = run.as_bytes();
    let has = |k| b.iter().any(|&c| class(c) == k);
    let switches = b.windows(2).filter(|w| class(w[0]) != class(w[1])).count();
    b.len() >= 24 && has(0) && has(1) && has(2) && switches * 5 >= b.len() * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_formats() {
        // built at runtime so secret scanners don't flag this file
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let aws = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let ant = format!("sk-ant-api03-{}", "x9Yz".repeat(6));
        assert_eq!(redact(&format!("token {gh} here")), "token [REDACTED] here");
        assert_eq!(redact(&format!("{aws}\n")), "[REDACTED]\n");
        // a JSON escape right before the token doesn't hide it
        assert_eq!(
            redact(&format!(r#"{{"text":"key:\n{ant}"}}"#)),
            r#"{"text":"key:\n[REDACTED]"}"#
        );
    }

    #[test]
    fn secret_pairs() {
        assert_eq!(
            redact("CLIENT_SECRET=hunter2hunter2"),
            "CLIENT_SECRET=[REDACTED]"
        );
        assert_eq!(
            redact(r#"client_secret = "abc""#),
            r#"client_secret = "[REDACTED]""#
        );
        assert_eq!(
            redact(r#"{"cmd":"{\"client_id\": \"my-app-1234\"}"}"#),
            r#"{"cmd":"{\"client_id\": \"[REDACTED]\"}"}"#
        );
        assert_eq!(redact("curl -d api_key=k3y"), "curl -d api_key=[REDACTED]");
    }

    #[test]
    fn high_entropy() {
        let s = r#"{"out":"value x7Kp2QmZ9vLw3RtY8bNc4HsJ6dFg1AeU5oPi0WqE end"}"#;
        assert_eq!(redact(s), r#"{"out":"value [REDACTED] end"}"#);
    }

    #[test]
    fn ordinary_code_untouched() {
        let s = r#"{"type":"assistant","session_id":"550e8400-e29b-41d4-a716-446655440000","usage":{"input_tokens":12345,"cache_read_input_tokens":987654}}
struct Creds { client_secret: String, password: Option<String> }
let secret = env::var("CLIENT_SECRET")?;
commit 9fceb02d0ae598e95dc970b74767f19372d61af8
/Users/udeshya/Documents/personal/yogan-swarm/target/debug/build/yogan-swarm/2ff909ae2a44e69e/out
HandleRequestWithRetryAndBackoff2 CARGO_BUILD_JOBS=9 aws secretsmanager get-secret-value
sk-learn is fine, so is disk-cleanup-1"#;
        assert_eq!(redact(s), s);
    }
}
