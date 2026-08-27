//! Secret redaction for error strings and logs.
//!
//! The operator's provider API key lives in the upstream URL — for dRPC
//! as a PATH segment (`…/ethereum/<key>`). Any error text derived from a
//! failed upstream request can therefore leak that key — a `reqwest`
//! transport error carries the full request URL, and any leaked string
//! is doubly dangerous here: method-check errors are stored in
//! `method_check.results[].error` and served from `/attestation/info`,
//! and the compose sets `public_logs=true` so container logs are
//! world-readable.
//!
//! [`Redactor`] scrubs error text on two independent axes so a key never
//! survives into a stored or logged string:
//!
//! 1. It replaces the exact secret value(s) — the raw key and its
//!    percent-encoded form — with `<redacted>`; this is the axis that
//!    catches a path-embedded key wherever it appears.
//! 2. It strips every `?…` query string from URL-shaped substrings — a
//!    generic backstop for query-parameter key shapes and any other
//!    sensitive query data.
//!
//! Callers should additionally use `reqwest::Error::without_url()` before
//! formatting a transport error; the redactor is the defence-in-depth
//! backstop that also covers non-transport paths.

/// Placeholder substituted for any redacted secret. Public so the boot
/// config can reject a `PROVIDER_API_KEY` that equals it (see
/// [`reject_marker_api_key`]).
pub const REDACTION_MARKER: &str = "<redacted>";

/// Reject a provider API key that literally equals the redaction marker.
///
/// If `PROVIDER_API_KEY` were exactly `<redacted>`, [`Redactor::redact`]
/// would "replace" the marker with itself — a no-op — so a key-echoing
/// upstream error would survive verbatim into a stored `results[].error`
/// (served from the public `/attestation/info`) or a world-readable log
/// line. Such a value is never a real provider key, so it is rejected
/// fail-closed at boot rather than silently defeating redaction.
///
/// # Errors
///
/// Returns an error message when `api_key == REDACTION_MARKER`.
pub fn reject_marker_api_key(api_key: &str) -> Result<(), String> {
    if api_key == REDACTION_MARKER {
        return Err(format!(
            "PROVIDER_API_KEY must not equal the redaction marker {REDACTION_MARKER:?} — \
             such a value is not a real provider key and would make error redaction a \
             no-op, letting a key-echoing upstream error survive verbatim"
        ));
    }
    Ok(())
}

/// Scrubs known secrets and URL query strings out of error/log text.
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Secret values to replace verbatim, longest first so a value that
    /// is a prefix of another does not partially mask it.
    secrets: Vec<String>,
}

impl Redactor {
    /// Build a redactor for `secrets`. Only **empty** values are dropped
    /// (an empty needle would match everywhere); every non-empty value is
    /// kept regardless of length. A short provider key must still be
    /// scrubbed — dropping it would let a 1–3 char key survive into a
    /// stored `results[].error` or a world-readable log line, which is
    /// exactly the leak this redactor exists to prevent. The (tiny)
    /// chance of masking an unrelated short substring is the correct
    /// trade against leaking a real key.
    #[must_use]
    pub fn new(secrets: &[&str]) -> Self {
        let mut secrets: Vec<String> = secrets
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| (*s).to_owned())
            .collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self { secrets }
    }

    /// Redact `input`: strip URL query strings, then mask any known
    /// secret value.
    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        let mut out = strip_query_strings(input);
        for secret in &self.secrets {
            if out.contains(secret) {
                out = out.replace(secret, REDACTION_MARKER);
            }
        }
        out
    }
}

/// Replace the query portion of any URL-shaped token with
/// `?<redacted>`. A "query" starts at the first `?` of a `scheme://`
/// token and runs to the next whitespace (URLs in error strings are not
/// whitespace-delimited internally). Text with no scheme is returned
/// unchanged so ordinary messages that happen to contain `?` are not
/// mangled.
fn strip_query_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(scheme_pos) = find_scheme(rest) {
        // Copy up to and including the scheme host/path, then find the
        // `?` that begins the query for this URL token.
        let after = &rest[scheme_pos..];
        // The URL token ends at the next ASCII whitespace.
        let token_end = after.find(char::is_whitespace).unwrap_or(after.len());
        let token = &after[..token_end];
        out.push_str(&rest[..scheme_pos]);
        if let Some(q) = token.find('?') {
            out.push_str(&token[..q]);
            out.push_str("?<redacted>");
        } else {
            out.push_str(token);
        }
        rest = &after[token_end..];
    }
    out.push_str(rest);
    out
}

/// Index of the start of the next `http://`, `https://`, `ws://`, or
/// `wss://` token in `s`, if any.
fn find_scheme(s: &str) -> Option<usize> {
    ["https://", "http://", "wss://", "ws://"]
        .iter()
        .filter_map(|scheme| s.find(scheme))
        .min()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const KEY: &str = "sk-live-drpc-DEADBEEF-cafef00d";

    #[test]
    fn redacts_exact_key_value() {
        let r = Redactor::new(&[KEY]);
        let msg = format!("upstream returned 401 for key {KEY} in body");
        let out = r.redact(&msg);
        assert!(!out.contains(KEY), "key must not survive: {out}");
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redacts_key_in_url_path() {
        // The dRPC upstream URL embeds the key as a path segment with no
        // query string — the exact-value axis must scrub it.
        let r = Redactor::new(&[KEY]);
        let msg =
            format!("transport error for url (https://lb.drpc.live/ethereum/{KEY}): timed out");
        let out = r.redact(&msg);
        assert!(!out.contains(KEY), "key must not survive: {out}");
        assert!(out.contains("/ethereum/<redacted>"), "{out}");
    }

    #[test]
    fn redacts_key_in_url_query() {
        // Query-shaped URLs (other providers / future entries) are still
        // stripped wholesale, even before the exact match runs.
        let r = Redactor::new(&[KEY]);
        let msg = format!("transport error for url (https://host/rpc?apikey={KEY}): timed out");
        let out = r.redact(&msg);
        assert!(!out.contains(KEY), "key must not survive: {out}");
        assert!(out.contains("/rpc?<redacted>"), "{out}");
    }

    #[test]
    fn redacts_percent_encoded_key() {
        // A key with characters OUTSIDE the unreserved set has a distinct
        // percent-encoded form; both spellings must be scrubbed.
        let raw = "k&y v/1";
        let encoded = crate::upstream::encode_api_key(raw);
        assert_ne!(raw, encoded);
        let r = Redactor::new(&[raw, &encoded]);
        let msg = format!("bad url wss://lb.drpc.live/ethereum/{encoded}");
        let out = r.redact(&msg);
        assert!(
            !out.contains(&encoded),
            "encoded key must not survive: {out}"
        );
        assert!(!out.contains(raw));
    }

    #[test]
    fn strips_query_even_without_known_secret() {
        // A redactor with no configured secrets still removes URL query
        // strings — the last line of defence if a key form is unknown.
        let r = Redactor::new(&[]);
        let out = r.redact("GET https://host/rpc?apikey=surprise-secret failed");
        assert!(!out.contains("surprise-secret"), "{out}");
        assert!(out.contains("/rpc?<redacted>"));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let r = Redactor::new(&[KEY]);
        let msg = "rpc error -32005: is this a query? maybe not";
        assert_eq!(r.redact(msg), msg);
    }

    #[test]
    fn short_keys_are_redacted_regardless_of_length() {
        // A short provider key must NOT slip through: any non-empty
        // needle is masked, even 1–3 chars. Leaking a real (short) key
        // into a stored error / world-readable log is worse than masking
        // an unrelated short substring.
        for key in ["a", "ab", "abc"] {
            let r = Redactor::new(&[key]);
            let out = r.redact(&format!("upstream 401 for key {key} here"));
            assert!(
                !out.contains(&format!(" {key} ")),
                "short key {key:?} must be redacted: {out}"
            );
            assert!(out.contains("<redacted>"), "{out}");
        }
    }

    #[test]
    fn empty_secret_is_dropped() {
        // An empty needle would match everywhere; it must be dropped so
        // ordinary text is untouched.
        let r = Redactor::new(&[""]);
        assert_eq!(r.redact("nothing to redact here"), "nothing to redact here");
    }

    #[test]
    fn rejects_api_key_equal_to_marker() {
        // A PROVIDER_API_KEY equal to the marker must be rejected at boot:
        // otherwise redaction is a no-op and a key-echoing error survives.
        assert!(reject_marker_api_key(REDACTION_MARKER).is_err());
        // A normal key is accepted.
        assert!(reject_marker_api_key(KEY).is_ok());
        // Demonstrate the collision the guard prevents: seeding a redactor
        // with the marker-as-key cannot scrub the marker from an error.
        let r = Redactor::new(&[REDACTION_MARKER]);
        let leak = format!("upstream 401 for key {REDACTION_MARKER} in body");
        assert!(
            r.redact(&leak).contains(REDACTION_MARKER),
            "replacing the marker with itself is a no-op — hence the boot guard"
        );
    }

    #[test]
    fn handles_multiple_urls_in_one_message() {
        let r = Redactor::new(&[]);
        let out = r.redact("a https://h1/rpc?apikey=k1 b wss://h2/ws?apikey=k2 c");
        assert!(!out.contains("k1") && !out.contains("k2"), "{out}");
        assert_eq!(
            out,
            "a https://h1/rpc?<redacted> b wss://h2/ws?<redacted> c"
        );
    }
}
