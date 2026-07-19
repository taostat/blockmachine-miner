//! Node-secret charset validation.
//!
//! `NODE_SECRET` (and the optional rotation secret `NODE_SECRET_NEXT`,
//! whose absent-or-empty value means "no rotation secret")
//! is the bearer token Envoy enforces on the proxied `/rpc` and `/ws`
//! routes only — the `/attestation/*` surface is public (Envoy strips any
//! inbound `Authorization` header before the bearer check). It is
//! operator-supplied and unmeasured, so it is treated strictly as
//! **data**: Envoy reads it from the process environment at request
//! time (never interpolated into config or script text), and both the
//! enclave and the registry constrain it to one narrow alphabet so the
//! two sides always agree on what a valid secret is.
//!
//! The allowed alphabet is `[A-Za-z0-9._-]`. It is deliberately free of
//! every character that is special to a shell, YAML, JSON, or a URL —
//! there is no context in which a conforming secret can escape a literal
//! or alter surrounding structure. attestd validates the secret at boot
//! and fails closed (the container restarts) on any violation, so a
//! measured, approved image never serves with a malformed secret.

use thiserror::Error;

/// The maximum accepted secret length. Long enough for a 256-bit secret
/// in any common encoding; bounded so a pathological value cannot bloat
/// logs or config.
const MAX_SECRET_LEN: usize = 256;

/// The minimum accepted secret length — a token this short has no
/// meaningful entropy and is almost certainly a misconfiguration.
const MIN_SECRET_LEN: usize = 8;

/// Why a node secret was rejected. The message never echoes the secret
/// value.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NodeSecretError {
    /// The secret was empty.
    #[error("{name} is empty")]
    Empty {
        /// The env var name (`NODE_SECRET` / `NODE_SECRET_NEXT`).
        name: &'static str,
    },
    /// The secret was shorter or longer than the accepted bounds.
    #[error(
        "{name} length {len} is outside the accepted range {MIN_SECRET_LEN}..={MAX_SECRET_LEN}"
    )]
    Length {
        /// The env var name.
        name: &'static str,
        /// The rejected length.
        len: usize,
    },
    /// The secret contained a character outside `[A-Za-z0-9._-]`.
    #[error("{name} contains a character outside the allowed alphabet [A-Za-z0-9._-]")]
    Charset {
        /// The env var name.
        name: &'static str,
    },
}

/// Return true when `b` is in the allowed secret alphabet
/// `[A-Za-z0-9._-]`.
#[must_use]
fn is_allowed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

/// Validate a required node secret. `name` is the env var name, used
/// only for the error message (never the value).
///
/// # Errors
///
/// Returns [`NodeSecretError`] when `secret` is empty, out of the
/// length bounds, or contains a character outside `[A-Za-z0-9._-]`.
pub fn validate_node_secret(name: &'static str, secret: &str) -> Result<(), NodeSecretError> {
    if secret.is_empty() {
        return Err(NodeSecretError::Empty { name });
    }
    if secret.len() < MIN_SECRET_LEN || secret.len() > MAX_SECRET_LEN {
        return Err(NodeSecretError::Length {
            name,
            len: secret.len(),
        });
    }
    if !secret.bytes().all(is_allowed) {
        return Err(NodeSecretError::Charset { name });
    }
    Ok(())
}

/// Validate an optional rotation secret. The rotation secret is
/// genuinely optional: an **absent** value (`None`) OR a **present but
/// empty** value (`Some("")`) both mean "no rotation secret" and are
/// accepted as a no-op. This matters because `NODE_SECRET_NEXT` is
/// always declared in the compose `environment` passthrough (so it is
/// part of the measured `allowed_envs`, keeping `compose_hash` stable
/// whether or not rotation is later used); a first deploy that sets only
/// `NODE_SECRET` therefore surfaces `NODE_SECRET_NEXT` to attestd as an
/// empty string, which must NOT fail the boot gate. A non-empty value is
/// still fully charset-validated by [`validate_node_secret`].
///
/// # Errors
///
/// Propagates [`validate_node_secret`] for a present, non-empty value.
pub fn validate_optional_node_secret(
    name: &'static str,
    secret: Option<&str>,
) -> Result<(), NodeSecretError> {
    match secret {
        // Absent or present-but-empty both mean "no rotation secret".
        None | Some("") => Ok(()),
        Some(value) => validate_node_secret(name, value),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn accepts_conforming_secret() {
        validate_node_secret("NODE_SECRET", "Abc123._-xyz").expect("valid secret");
        validate_node_secret("NODE_SECRET", &"a".repeat(64)).expect("valid long secret");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(
            validate_node_secret("NODE_SECRET", ""),
            Err(NodeSecretError::Empty {
                name: "NODE_SECRET"
            })
        );
    }

    #[test]
    fn rejects_too_short() {
        assert!(matches!(
            validate_node_secret("NODE_SECRET", "short"),
            Err(NodeSecretError::Length { .. })
        ));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(MAX_SECRET_LEN + 1);
        assert!(matches!(
            validate_node_secret("NODE_SECRET", &long),
            Err(NodeSecretError::Length { .. })
        ));
    }

    #[test]
    fn rejects_lua_breakout_characters() {
        // The exact class of value the injection fix must exclude: a
        // secret that would otherwise close a Lua string literal and
        // append code disabling auth.
        for evil in [
            "abc\"; local ok=true --",
            "key with space",
            "key\nnewline",
            "quote\"inside",
            "back\\slash",
            "semi;colon",
            "brace}here",
            "t+plus/eq=",
        ] {
            assert_eq!(
                validate_node_secret("NODE_SECRET", evil),
                Err(NodeSecretError::Charset {
                    name: "NODE_SECRET"
                }),
                "must reject {evil:?}"
            );
        }
    }

    #[test]
    fn optional_secret_absent_is_ok() {
        validate_optional_node_secret("NODE_SECRET_NEXT", None).expect("absent is fine");
    }

    #[test]
    fn optional_secret_empty_is_no_op() {
        // A first deploy sets only NODE_SECRET; the compose still declares
        // NODE_SECRET_NEXT in its measured passthrough, so attestd sees it
        // as an empty string. That must be accepted as "no rotation
        // secret" (not rejected as Empty) so the default deploy boots and
        // reproduces the approved compose_hash.
        validate_optional_node_secret("NODE_SECRET_NEXT", Some("")).expect("empty is a no-op");
    }

    #[test]
    fn optional_secret_present_is_validated() {
        // A non-empty rotation secret is still fully charset-checked.
        assert!(validate_optional_node_secret("NODE_SECRET_NEXT", Some("bad char!")).is_err());
        assert!(matches!(
            validate_optional_node_secret("NODE_SECRET_NEXT", Some("x")),
            Err(NodeSecretError::Length { .. })
        ));
        validate_optional_node_secret("NODE_SECRET_NEXT", Some("Rotation_2.next-1")).unwrap();
    }
}
