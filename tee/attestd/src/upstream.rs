//! Hardcoded provider upstream base URLs per (chain, provider).
//!
//! The provider base URLs are baked into the image source — here for
//! attestd's method checker and in `image/start.sh` for Envoy's
//! upstream cluster — so they are inside the measured `compose_hash`.
//! A miner cannot point the proxy at a different upstream without
//! changing the image, which moves the hash and is rejected by the
//! registry's `tee_image_versions` allowlist.
//!
//! Phase 1: dRPC ETH only. Adding a provider/chain is a new
//! hardcoded entry + a new image release + new `tee_image_versions`
//! rows (see the contract doc's "Provider allowlist" section).

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use thiserror::Error;

/// Bytes percent-escaped in the operator API key: everything outside
/// `[A-Za-z0-9-._~]` (the RFC 3986 unreserved set). The unreserved
/// punctuation must stay LITERAL — dRPC embeds the key as a URL *path*
/// segment and does not percent-decode it, so an escaped `-` (`%2D`)
/// is rejected as an unknown token.
const API_KEY_ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode the operator's API key before it is placed in a URL.
/// Every byte outside the RFC 3986 unreserved set (`[A-Za-z0-9-._~]`)
/// is percent-escaped, so a key containing `/`, `?`, `#`, `&`,
/// whitespace, or any other reserved character cannot traverse the
/// path, start a query string, or otherwise alter the request target —
/// while real provider keys (which use the unreserved alphabet) pass
/// through byte-identical. The Envoy Lua filter applies the identical
/// rule (`gsub("[^%w_%-%.~]", ...)` — Lua `%w` is alphanumeric only, so
/// `_` is listed explicitly) so attestd's probe URL and Envoy's proxied
/// URL resolve to the exact same upstream target.
#[must_use]
pub fn encode_api_key(api_key: &str) -> String {
    utf8_percent_encode(api_key, API_KEY_ESCAPE_SET).to_string()
}

/// A resolved provider upstream: the HTTP JSON-RPC base URL and the
/// WebSocket base URL, both carrying the operator's API key.
#[derive(Debug, Clone)]
pub struct ProviderUpstream {
    /// HTTP JSON-RPC endpoint, e.g. `https://lb.drpc.live/ethereum/<key>`.
    pub http_url: String,
    /// WebSocket endpoint, e.g. `wss://lb.drpc.live/ethereum/<key>`.
    pub ws_url: String,
    /// The upstream TLS host, e.g. `lb.drpc.live`. Matches the
    /// host `start.sh` renders into Envoy's provider cluster.
    pub host: String,
}

/// Error for an unknown (chain, provider) combination. Fatal at boot:
/// the container must refuse to start rather than proxy to an
/// unapproved upstream.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unsupported (chain, provider) combination: ({chain}, {provider})")]
pub struct UnknownUpstream {
    /// The requested chain slug.
    pub chain: String,
    /// The requested provider slug.
    pub provider: String,
}

/// Resolve the hardcoded upstream base URLs for `(chain, provider)`,
/// substituting the operator's `api_key` into the URL. dRPC embeds the
/// key as the final path segment (`/ethereum/<key>`), identical for the
/// HTTP and WebSocket endpoints.
///
/// # Errors
///
/// Returns [`UnknownUpstream`] for any combination not baked into this
/// release. The caller fails fast — the container exits and never
/// serves.
pub fn resolve_upstream(
    chain: &str,
    provider: &str,
    api_key: &str,
) -> Result<ProviderUpstream, UnknownUpstream> {
    let key = encode_api_key(api_key);
    match (chain, provider) {
        ("eth", "drpc") => Ok(ProviderUpstream {
            http_url: format!("https://lb.drpc.live/ethereum/{key}"),
            ws_url: format!("wss://lb.drpc.live/ethereum/{key}"),
            host: "lb.drpc.live".to_owned(),
        }),
        _ => Err(UnknownUpstream {
            chain: chain.to_owned(),
            provider: provider.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn resolves_drpc_eth() {
        // A real-shaped dRPC key (unreserved alphabet, incl. hyphens)
        // passes through byte-identical — dRPC does not percent-decode
        // the path segment, so an escaped hyphen would be rejected.
        let up = resolve_upstream("eth", "drpc", "Avl7-test_Key.123~x").unwrap();
        assert_eq!(
            up.http_url,
            "https://lb.drpc.live/ethereum/Avl7-test_Key.123~x"
        );
        assert_eq!(up.ws_url, "wss://lb.drpc.live/ethereum/Avl7-test_Key.123~x");
        assert_eq!(up.host, "lb.drpc.live");
    }

    #[test]
    fn api_key_is_percent_encoded_in_urls() {
        // A key carrying reserved URL characters must be escaped so it
        // cannot traverse the path, start a query string, or otherwise
        // alter the request target.
        let up = resolve_upstream("eth", "drpc", "a&b=c?d #e/f").unwrap();
        assert_eq!(
            up.http_url,
            "https://lb.drpc.live/ethereum/a%26b%3Dc%3Fd%20%23e%2Ff"
        );
        assert_eq!(
            up.ws_url,
            "wss://lb.drpc.live/ethereum/a%26b%3Dc%3Fd%20%23e%2Ff"
        );
        // The encoded key segment carries no raw reserved character, so
        // it cannot escape its path segment.
        let key_segment = up
            .http_url
            .rsplit_once("/ethereum/")
            .map(|(_, v)| v)
            .expect("key path segment present");
        for reserved in ['&', '=', '?', '#', ' ', '/'] {
            assert!(
                !key_segment.contains(reserved),
                "reserved char {reserved:?} must not appear raw in the key segment: {key_segment}"
            );
        }
    }

    #[test]
    fn encode_api_key_escapes_reserved_keeps_unreserved() {
        assert_eq!(encode_api_key("abcXYZ012"), "abcXYZ012");
        // RFC 3986 unreserved punctuation stays literal (dRPC keys use it).
        assert_eq!(encode_api_key("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_api_key("k&y=v"), "k%26y%3Dv");
        assert_eq!(encode_api_key("../x"), "..%2Fx");
        assert_eq!(encode_api_key("k%00"), "k%2500");
    }

    #[test]
    fn rejects_unknown_chain() {
        let err = resolve_upstream("bsc", "drpc", "k").unwrap_err();
        assert_eq!(err.chain, "bsc");
        assert_eq!(err.provider, "drpc");
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!(resolve_upstream("eth", "alchemy", "k").is_err());
        assert!(resolve_upstream("eth", "onfinality", "k").is_err());
        assert!(resolve_upstream("eth", "", "k").is_err());
    }

    #[test]
    fn rejects_case_variants() {
        // Chain/provider slugs are canonical lower-case on the wire and
        // in the compose; anything else is a configuration error, not
        // something to normalise silently.
        assert!(resolve_upstream("ETH", "drpc", "k").is_err());
        assert!(resolve_upstream("eth", "dRPC", "k").is_err());
    }
}
