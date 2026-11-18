//! Logging goes through `tracing` with target "grvt". With the client, `GRVT_LOG=debug` (or any level)
//! installs a stderr subscriber with the filter `grvt=<level>` when the application has not set one itself.
//!
//! Wire logs are always redacted: the `gravity` cookie, api keys, private keys, signature r/s and
//! any 32-byte hex value.

#[cfg(feature = "client")]
use std::sync::Once;

const SECRET_KEYS: [&str; 6] = ["api_key", "private_key", "r", "s", "gravity", "cookie"];

#[cfg(feature = "client")]
static INIT: Once = Once::new();

/// Reads `GRVT_LOG` once and installs a subscriber if it is set and none is installed (client only: the
/// core never installs a subscriber).
#[cfg(feature = "client")]
pub fn configure_from_env() {
    INIT.call_once(|| {
        let level = match std::env::var("GRVT_LOG") {
            Ok(v) if !v.is_empty() => v.to_lowercase(),
            _ => return,
        };
        let filter = tracing_subscriber::EnvFilter::try_new(format!("grvt={level}"))
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("grvt=info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    });
}

#[inline]
pub fn debug_enabled() -> bool {
    tracing::enabled!(target: "grvt", tracing::Level::DEBUG)
}

/// Header values that are secrets.
pub fn redact_header(name: &str, value: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "cookie" | "set-cookie" | "authorization" => "***".into(),
        _ => value.to_owned(),
    }
}

/// Redact secrets in a JSON body, cookie string or log line. Truncates to `max` bytes first.
pub fn redact(data: &[u8], max: usize) -> String {
    let data = &data[..data.len().min(max)];
    let s = String::from_utf8_lossy(data);
    let s = redact_json_values(&s);
    let s = redact_cookie(&s);
    redact_hex_keys(&s)
}

fn redact_json_values(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            if let Some(end) = s[i + 1..].find('"').map(|e| i + 1 + e) {
                let key = &s[i + 1..end];
                let mut j = end + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if SECRET_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)) && j < b.len() && b[j] == b':' {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'"' {
                        if let Some(vend) = s[j + 1..].find('"').map(|e| j + 1 + e) {
                            out.push_str(&s[i..j]);
                            out.push_str("\"***\"");
                            i = vend + 1;
                            continue;
                        }
                    }
                }
                out.push_str(&s[i..=end]);
                i = end + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

fn redact_cookie(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("gravity=") {
        out.push_str(&rest[..p + 8]);
        let tail = &rest[p + 8..];
        let end = tail
            .find(|c: char| c == ';' || c == '"' || c.is_whitespace())
            .unwrap_or(tail.len());
        out.push_str("***");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn redact_hex_keys(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if boundary && b[i..].starts_with(b"0x") && b.len() >= i + 66 {
            let hex = &b[i + 2..i + 66];
            let after_ok = b.get(i + 66).is_none_or(|c| !c.is_ascii_alphanumeric());
            if after_ok && hex.iter().all(u8::is_ascii_hexdigit) {
                out.push_str("0x***");
                i += 66;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        let key = format!("0x{}", "ab".repeat(32));
        let line = format!(
            r#"{{"api_key": "k-123", "r":"0x1","order":{{"s" : "0x2","size":"1.5"}},"pk":"{key}"}} gravity=abc; Path=/"#
        );
        let out = redact(line.as_bytes(), 10_000);
        assert!(
            !out.contains("k-123") && !out.contains("abc;") && !out.contains(&key),
            "{out}"
        );
        assert!(out.contains(r#""size":"1.5""#) && out.contains("gravity=***;"), "{out}");
        assert_eq!(redact_header("Cookie", "gravity=x"), "***");
    }
}
