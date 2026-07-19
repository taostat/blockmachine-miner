#!/usr/bin/env python3
"""Offline compose_hash derivation for the BM TEE proxy-miner image.

Python port of gm-miner's ``cli/src/compose_hash.rs``.

What ``compose_hash`` is
------------------------
A dstack CVM's ``compose_hash`` is sha256 over the canonical
serialization of its ``app_compose`` object — the wrapper dstack
measures into RTMR3 and exposes in the attestation TCB info. The VMM
hashes the exact UTF-8 bytes of the submitted ``app-compose.json``
string, and the guest re-hashes the same file, so the value is
re-derivable offline. This script mirrors that canonical serialization
so approved hashes can be computed in CI and published to the
registry's ``/admin/tee-image-versions`` before release, instead of
deploying a CVM to read the hash back.

The serialization (mirrors dstack's ``get_compose_hash``)
---------------------------------------------------------
``app_compose`` is serialized as JSON with lexicographically sorted
keys and compact separators (``,`` and ``:``, no spaces), non-ASCII
left as UTF-8 (``ensure_ascii=False``), and the digest is lowercase
hex over the UTF-8 bytes.

The ``app_compose`` field set
-----------------------------
The hashed object is the WRAPPER, not the raw ``docker-compose.yaml``.
``docker_compose_file`` carries the rendered compose YAML as one JSON
string; ``pre_launch_script`` carries the bundled pre-launch script.
The remaining fields are the Phala-Cloud-set security/runtime flags
plus ``allowed_envs`` — pinned to the exact flag set gm-miner's
``compose_hash.rs`` reproduces against a live registry-approved hash.

Pinning + golden fixture
------------------------
The tool pins the exact ``phala`` CLI version (``PHALA_CLI_VERSION``) it
mirrors and the dstack OS image (``OS_IMAGE_NAME`` / ``OS_IMAGE_HASH``)
the release runs on, so CI cannot silently drift onto a newer CLI or a
different base image. ``os_image_hash`` is a bring-up dependency (real
value from Phala's published metadata; ``None`` until then) and — being
bound by the boot measurement set, not the compose — is not part of
``compose_hash``. A byte-for-byte check against a real ``phala prepare``
output requires a real deploy artifact: drop it into ``testdata/`` (see
``GOLDEN_APP_COMPOSE``) and the self-test locks the hash; until then that
test is skipped. The tool never fabricates a golden hash.

Usage
-----
    # Print the compose_hash for a release:
    ./derive_compose_hash.py --image-ref ghcr.io/taostat/bm-tee-miner@sha256:... \
        --chain eth --provider drpc

    # Print the rendered docker-compose.yaml instead:
    ./derive_compose_hash.py --image-ref ... --chain eth --provider drpc --render

    # Self-test (also runnable via pytest):
    ./derive_compose_hash.py --self-test
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

# Directory layout: tee/scripts/derive_compose_hash.py next to
# tee/dstack/{docker-compose.yaml,prelaunch.sh}.
DEFAULT_COMPOSE_TEMPLATE = Path(__file__).resolve().parent.parent / "dstack" / "docker-compose.yaml"
DEFAULT_PRELAUNCH_SCRIPT = Path(__file__).resolve().parent.parent / "dstack" / "prelaunch.sh"

# ── Release pins (see the "Pinning + golden fixture" docstring section) ─
# The exact Phala CLI version the release is prepared and measured with.
# The canonical app_compose field set + serialization this tool mirrors
# is CLI-version specific, so pinning the version is what keeps CI from
# silently drifting onto a newer CLI that reorders or adds fields (which
# would move the hash). Bump this together with the golden fixture below.
PHALA_CLI_VERSION = "0.1.15"

# The dstack OS image this release runs on. NOTE: `os_image_hash` is NOT
# part of `compose_hash` — the OS image is bound separately, via the boot
# measurement set (`mr_td`, `rtmr0..2`) recorded on the
# `tee_image_versions` row. These constants are pinned here only so the
# release, this tool, and the registry row all agree on the exact base
# image. `OS_IMAGE_HASH` stays `None` until it is populated from Phala's
# published image metadata at release/bring-up time — a real value cannot
# be produced offline, and this tool never fabricates one.
OS_IMAGE_NAME = "dstack-0.5.3"
OS_IMAGE_HASH: str | None = None

# Golden fixture (bring-up dependency). A byte-for-byte check that this
# tool's canonical serialization matches a REAL `phala prepare` output
# requires a real deploy artifact, which cannot be produced in a dev
# environment. To lock it once available:
#   1. Run `phala prepare` (CLI == PHALA_CLI_VERSION) for the release and
#      capture the submitted `app-compose.json` verbatim.
#   2. Save its bytes to GOLDEN_APP_COMPOSE and the deploy's reported
#      compose_hash to GOLDEN_COMPOSE_HASH.
# `test_matches_golden_app_compose_when_present` then asserts our
# canonicalization reproduces that exact hash; until the files exist it
# is skipped (offline-safe). Do NOT hand-write these — a fabricated
# golden proves nothing.
_TESTDATA_DIR = Path(__file__).resolve().parent / "testdata"
GOLDEN_APP_COMPOSE = _TESTDATA_DIR / "golden_app_compose.json"
GOLDEN_COMPOSE_HASH = _TESTDATA_DIR / "golden_compose_hash.txt"

# The `manifest_version` of the app_compose format dstack currently emits.
MANIFEST_VERSION = 2

# The `runner` a docker-compose CVM uses.
RUNNER = "docker-compose"

# The `storage_fs` the Phala node provisions for the pinned OS image.
STORAGE_FS = "zfs"

# The `features` array Phala Cloud sets for a KMS-backed,
# gateway-fronted CVM.
FEATURES = ["kms", "tproxy-net"]

# The env-var names a BM TEE release deploy declares. The hash covers
# names only, not values, so every miner produces the same
# compose_hash. Must match the `environment` passthrough list in
# dstack/docker-compose.yaml (rendered literals CHAIN/PROVIDER are not
# operator envs and do not appear here).
#
# NODE_SECRET_NEXT is ALWAYS listed even though the rotation secret is
# optional. Phala derives the measured `allowed_envs` from the KEYS
# declared in the deploy `.env`, so the deploy MUST declare all three
# keys for its compose_hash to match this set — the README's first-deploy
# `.env` therefore lists `NODE_SECRET_NEXT=` with an empty value even
# when rotation is off. An empty value still counts as a declared key
# (the hash covers names, not values), and attestd treats an absent/empty
# NODE_SECRET_NEXT as "no rotation secret" so that same deploy also boots.
# Omitting the key from the `.env` (not just leaving it empty) would drop
# it from `allowed_envs` and move the hash — keep the tool, the compose
# passthrough, and the documented `.env` in lockstep on all three keys.
CANONICAL_ALLOWED_ENVS = [
    "PROVIDER_API_KEY",
    "NODE_SECRET",
    "NODE_SECRET_NEXT",
]

# The pinned app_compose security and runtime flag fields a release
# deploy produces. Copied exactly from gm-miner's compose_hash.rs
# RELEASE_FLAGS — that pinning is anchored there against a live
# registry-approved hash. `gateway_enabled` and `tproxy_enabled` are
# both present and true (dstack accepts either name; the Phala backend
# emits both); the local key provider is off because the CVM uses
# Phala's KMS.
RELEASE_FLAGS = {
    "kms_enabled": True,
    "gateway_enabled": True,
    "tproxy_enabled": True,
    "local_key_provider_enabled": False,
    "public_logs": True,
    "public_sysinfo": True,
    "public_tcbinfo": True,
    "secure_time": False,
    "no_instance_id": False,
}

# Matches the `${BM_TEE_IMAGE_REF:?...}` placeholder in the compose
# template.
IMAGE_REF_PLACEHOLDER = re.compile(r"\$\{BM_TEE_IMAGE_REF[^}]*\}")

# A digest-pinned OCI image ref: `host[:port]/repo[:tag]@sha256:<64
# lowercase hex>`. A substring test for "@sha256:" accepts malformed
# digests (`img@sha256:deadbeef`, uppercase, wrong algo); a permissive
# char class additionally accepts an empty repo (`ghcr.io/@sha256:...`)
# and UPPERCASE repo names, neither of which is a valid OCI name. This
# full-match grammar enforces the real OCI reference shape:
#   - host: a dotted registry FQDN (optional `:port`), or a single
#     component WITH a port (e.g. `localhost:5000`). A bare single
#     component is not a valid host — which also rejects the empty-repo
#     form `ghcr.io/@sha256:...` (host `ghcr.io`, repo empty).
#   - repo: one-or-more LOWERCASE OCI path components
#     (`[a-z0-9]+([._-][a-z0-9]+)*`), `/`-separated — no uppercase, no
#     newlines or shell/YAML metacharacters.
#   - optional `:tag`.
#   - digest: EXACTLY `sha256:` + 64 LOWERCASE hex chars.
_OCI_HOST_COMPONENT = r"[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?"
_OCI_HOST = (
    rf"(?:{_OCI_HOST_COMPONENT}(?:\.{_OCI_HOST_COMPONENT})+(?::[0-9]+)?"
    rf"|{_OCI_HOST_COMPONENT}:[0-9]+)"
)
_OCI_PATH_COMPONENT = r"[a-z0-9]+(?:[._-][a-z0-9]+)*"
_OCI_REPO = rf"{_OCI_PATH_COMPONENT}(?:/{_OCI_PATH_COMPONENT})*"
_OCI_TAG = r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}"
IMAGE_REF_RE = re.compile(
    rf"{_OCI_HOST}/{_OCI_REPO}(?::{_OCI_TAG})?@sha256:[0-9a-f]{{64}}"
)

# Chains/providers this phase-1 tool can render into a measured compose.
# BSC is a separate rollout with its own image release; reject it here
# rather than silently bake an unmeasured/unsupported compose. Adding a
# chain/provider means a new image release + new tee_image_versions rows.
ALLOWED_CHAINS = ("eth",)
ALLOWED_PROVIDERS = ("drpc",)

# Any ASCII control char (newline, CR, tab, NUL, DEL, …). An input
# carrying one could inject extra lines into the rendered compose YAML
# (and thus silently move — or corrupt — the measured compose_hash).
_CONTROL_CHARS_RE = re.compile(r"[\x00-\x1f\x7f]")


def _reject_control_chars(name: str, value: str) -> None:
    """Raise if `value` carries a newline or other control character."""
    if _CONTROL_CHARS_RE.search(value):
        raise ValueError(f"{name} must not contain newlines or control characters")


def validate_release_inputs(image_ref: str, chain: str, provider: str) -> None:
    """Validate CLI inputs before they are rendered into the compose YAML.

    A malformed image digest, a newline/control character in any input,
    or an off-allowlist chain/provider is rejected outright — a bad input
    must never be silently baked into a measured compose (the hash the
    registry approves).

    Raises:
        ValueError: on any invalid input.
    """
    _reject_control_chars("--image-ref", image_ref)
    _reject_control_chars("--chain", chain)
    _reject_control_chars("--provider", provider)
    if not IMAGE_REF_RE.fullmatch(image_ref):
        raise ValueError(
            "--image-ref must be digest-pinned as image@sha256:<64 lowercase hex> "
            f"(got {image_ref!r})"
        )
    if chain not in ALLOWED_CHAINS:
        raise ValueError(f"--chain must be one of {ALLOWED_CHAINS} (got {chain!r})")
    if provider not in ALLOWED_PROVIDERS:
        raise ValueError(f"--provider must be one of {ALLOWED_PROVIDERS} (got {provider!r})")


def render_compose(template: str, image_ref: str, chain: str, provider: str) -> str:
    """Render the compose template the way the deploy tooling does.

    Substitutes the digest-pinned image ref for the
    ``${BM_TEE_IMAGE_REF:?...}`` placeholder and the chain/provider
    slugs for the ``__CHAIN__`` / ``__PROVIDER__`` rendered-literal
    placeholders. Raises if a placeholder is missing or survives —
    either is a template bug that would silently move the hash.
    """
    if not IMAGE_REF_PLACEHOLDER.search(template):
        raise ValueError("compose template is missing the ${BM_TEE_IMAGE_REF} placeholder")
    if "__CHAIN__" not in template or "__PROVIDER__" not in template:
        raise ValueError("compose template is missing the __CHAIN__/__PROVIDER__ placeholders")
    rendered = IMAGE_REF_PLACEHOLDER.sub(image_ref, template)
    rendered = rendered.replace("__CHAIN__", chain).replace("__PROVIDER__", provider)
    if "__CHAIN__" in rendered or "__PROVIDER__" in rendered:
        raise ValueError("compose template placeholders survived rendering")
    return rendered


def build_app_compose(
    image_ref: str,
    chain: str,
    provider: str,
    compose_template: str,
    prelaunch_script: str,
) -> dict:
    """Build the app_compose wrapper a release deploy produces.

    Validates the release inputs FIRST: this is the single
    choke point every hash-producing path funnels through
    (``compute_compose_hash`` calls it), so imported/CI callers — not just
    the CLI — reject a malformed digest, a control char, or an
    off-allowlist chain/provider before it is ever baked into a measured
    compose. A bad input must never reach the hash.

    Raises:
        ValueError: on any invalid release input.
    """
    validate_release_inputs(image_ref, chain, provider)
    compose = {
        "allowed_envs": list(CANONICAL_ALLOWED_ENVS),
        "docker_compose_file": render_compose(compose_template, image_ref, chain, provider),
        "features": list(FEATURES),
        "manifest_version": MANIFEST_VERSION,
        "name": "",
        "pre_launch_script": prelaunch_script,
        "runner": RUNNER,
        "storage_fs": STORAGE_FS,
    }
    compose.update(RELEASE_FLAGS)
    return compose


def canonical_json(obj: dict) -> str:
    """dstack's canonical serialization: sorted keys, compact
    separators, non-ASCII left as UTF-8."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def hash_app_compose(compose: dict) -> str:
    """sha256 lowercase hex over the canonical UTF-8 bytes."""
    return hashlib.sha256(canonical_json(compose).encode("utf-8")).hexdigest()


def compute_compose_hash(
    image_ref: str,
    chain: str,
    provider: str,
    compose_template: str,
    prelaunch_script: str,
) -> str:
    """Compute the compose_hash a release deploy produces, offline."""
    return hash_app_compose(
        build_app_compose(image_ref, chain, provider, compose_template, prelaunch_script)
    )


def deploy_image_pin_hint() -> str:
    """The `phala deploy` OS-image pin the release MUST use.

    The boot measurement set (`mr_td`, `rtmr0..2`) is bound to the exact
    dstack base image, NOT to `compose_hash`, so `compose_hash` alone does
    not constrain which OS image the deploy boots. Emitting the pinned
    `OS_IMAGE_NAME` here keeps the documented deploy (`phala deploy
    --image ...`) and the registry's approved boot measurements on the
    same base image — omit the flag and Phala picks a default image whose
    measurements fall off the allowlist.
    """
    return f"phala deploy --image {OS_IMAGE_NAME}  # pin the OS image; boot measurements are bound to it"


# ── Tests (pytest-discoverable; also run by --self-test) ──────────────

# A tiny hand-checked fixture proving the canonicalization: the
# expected string is written out by hand and the sha256 was computed
# independently (python: hashlib.sha256('{"a":"ü","b":1,"c":true}'
# .encode()).hexdigest()).
_FIXTURE_OBJ = {"b": 1, "a": "ü", "c": True}
_FIXTURE_CANONICAL = '{"a":"ü","b":1,"c":true}'
_FIXTURE_SHA256 = "7a0366b9ca84d32815a679350bdd82dbd37dfffecf27ecd695fc9bb51c924dcd"

_TEST_TEMPLATE = (
    "services:\n"
    "  bm-tee-miner:\n"
    "    image: ${BM_TEE_IMAGE_REF:?must be set}\n"
    "    environment:\n"
    "      - CHAIN=__CHAIN__\n"
    "      - PROVIDER=__PROVIDER__\n"
)

# A valid digest-pinned ref for the hashing tests: build_app_compose /
# compute_compose_hash validate their inputs, so test inputs
# must be real OCI refs on the allowlist.
_VALID_REF = "ghcr.io/x@sha256:" + "0" * 64
_VALID_REF_ALT = "ghcr.io/x@sha256:" + "1" * 64


def test_canonical_json_matches_hand_computed_fixture():
    canon = canonical_json(_FIXTURE_OBJ)
    assert canon == _FIXTURE_CANONICAL, canon
    got = hashlib.sha256(canon.encode("utf-8")).hexdigest()
    assert got == _FIXTURE_SHA256, got


def test_canonical_json_is_sorted_and_compact():
    compose = build_app_compose(_VALID_REF, "eth", "drpc", _TEST_TEMPLATE, "#!/bin/sh\n")
    canon = canonical_json(compose)
    assert canon.startswith('{"allowed_envs":['), canon[:40]
    assert '", "' not in canon
    assert '": "' not in canon


def test_render_substitutes_all_placeholders():
    rendered = render_compose(_TEST_TEMPLATE, "ghcr.io/x@sha256:abc", "eth", "drpc")
    assert "ghcr.io/x@sha256:abc" in rendered
    assert "CHAIN=eth" in rendered
    assert "PROVIDER=drpc" in rendered
    assert "__CHAIN__" not in rendered and "BM_TEE_IMAGE_REF" not in rendered


def test_render_rejects_template_without_placeholders():
    for broken in ("services: {}", _TEST_TEMPLATE.replace("__CHAIN__", "eth")):
        try:
            render_compose(broken, "img", "eth", "drpc")
        except ValueError:
            continue
        raise AssertionError(f"expected ValueError for template: {broken!r}")


def test_hash_changes_with_inputs():
    args = (_VALID_REF, "eth", "drpc", _TEST_TEMPLATE, "#!/bin/sh\n")
    base = compute_compose_hash(*args)
    assert len(base) == 64 and base == base.lower()
    # A different (valid) image digest moves the hash.
    assert compute_compose_hash(_VALID_REF_ALT, *args[1:]) != base
    # A different prelaunch script moves the hash.
    assert compute_compose_hash(*args[:4], "#!/bin/sh\necho x\n") != base
    # Deterministic.
    assert compute_compose_hash(*args) == base
    # An off-allowlist chain is no longer silently hashable: the core
    # hashing path validates inputs, so bsc (a separate image
    # release) raises rather than producing a compose_hash.
    try:
        compute_compose_hash(args[0], "bsc", *args[2:])
    except ValueError:
        pass
    else:
        raise AssertionError("expected ValueError hashing an off-allowlist chain")


def test_app_compose_carries_the_pinned_flags():
    compose = build_app_compose(_VALID_REF, "eth", "drpc", _TEST_TEMPLATE, "#!/bin/sh\n")
    for key, expected in RELEASE_FLAGS.items():
        assert compose[key] is expected, key
    assert compose["allowed_envs"] == ["PROVIDER_API_KEY", "NODE_SECRET", "NODE_SECRET_NEXT"]
    assert compose["manifest_version"] == 2
    assert compose["runner"] == "docker-compose"
    assert compose["storage_fs"] == "zfs"
    assert compose["features"] == ["kms", "tproxy-net"]
    assert compose["name"] == ""


def test_repo_compose_template_renders_and_hashes():
    template = DEFAULT_COMPOSE_TEMPLATE.read_text(encoding="utf-8")
    prelaunch = DEFAULT_PRELAUNCH_SCRIPT.read_text(encoding="utf-8")
    digest = compute_compose_hash(
        "ghcr.io/taostat/bm-tee-miner@sha256:" + "0" * 64,
        "eth",
        "drpc",
        template,
        prelaunch,
    )
    assert re.fullmatch(r"[0-9a-f]{64}", digest)


def test_release_pins_are_declared():
    # The pins are the single source of truth CI reads; assert their
    # shape so a malformed edit fails the self-test rather than silently
    # publishing a wrong ref.
    assert re.fullmatch(r"\d+\.\d+\.\d+", PHALA_CLI_VERSION), PHALA_CLI_VERSION
    assert OS_IMAGE_NAME.startswith("dstack-"), OS_IMAGE_NAME
    # os_image_hash is a bring-up dependency: None until populated from
    # Phala's published metadata. Assert only its shape when present, and
    # never accept a fabricated/short value.
    assert OS_IMAGE_HASH is None or re.fullmatch(r"[0-9a-f]{64}", OS_IMAGE_HASH), OS_IMAGE_HASH


def test_deploy_hint_pins_os_image():
    # The emitted deploy hint must carry the exact `--image OS_IMAGE_NAME`
    # so operators pin the OS image the release was measured on. This is
    # the boot-measurement half of the approval that `compose_hash` alone
    # does not cover.
    hint = deploy_image_pin_hint()
    assert f"--image {OS_IMAGE_NAME}" in hint, hint


def test_readme_documents_the_os_image_pin():
    # Consistency guard: the documented `phala deploy` command must pin
    # the same OS image this tool emits, so the docs, the tool, and the
    # registry's approved boot measurements cannot silently drift apart.
    readme = Path(__file__).resolve().parent.parent / "README.md"
    text = readme.read_text(encoding="utf-8")
    assert f"--image {OS_IMAGE_NAME}" in text, f"README must pin --image {OS_IMAGE_NAME}"


def test_validate_rejects_malformed_digest():
    # Only a full `@sha256:<64 lowercase hex>` passes; the old substring
    # test for "@sha256:" accepted every one of these.
    for bad in (
        "ghcr.io/x@sha256:deadbeef",  # too short
        "ghcr.io/x@sha256:" + "g" * 64,  # non-hex
        "ghcr.io/x@sha256:" + "A" * 64,  # uppercase (dstack digests are lowercase)
        "ghcr.io/x@sha256:" + "0" * 63,  # 63 chars
        "ghcr.io/x@sha256:" + "0" * 65,  # 65 chars
        "ghcr.io/x:latest",  # no digest at all
        "ghcr.io/x@sha512:" + "0" * 64,  # wrong algorithm
        "@sha256:" + "0" * 64,  # empty repo
    ):
        try:
            validate_release_inputs(bad, "eth", "drpc")
        except ValueError:
            continue
        raise AssertionError(f"expected ValueError for malformed image ref {bad!r}")


def test_validate_rejects_newline_or_control_char_in_inputs():
    valid = "ghcr.io/x@sha256:" + "0" * 64
    # A newline (or CR/NUL) in ANY input could inject extra compose YAML.
    bad_inputs = [
        (valid + "\n", "eth", "drpc"),
        (valid, "eth\n", "drpc"),
        (valid, "eth", "drpc\n  extra: true"),
        (valid, "eth", "drpc\r"),
        (valid + "\x00", "eth", "drpc"),
        (valid, "eth\t", "drpc"),
    ]
    for image_ref, chain, provider in bad_inputs:
        try:
            validate_release_inputs(image_ref, chain, provider)
        except ValueError:
            continue
        raise AssertionError(
            f"expected ValueError for control char in {(image_ref, chain, provider)!r}"
        )


def test_validate_rejects_off_allowlist_chain_provider():
    valid = "ghcr.io/x@sha256:" + "0" * 64
    for chain, provider in (
        ("bsc", "drpc"),  # separate rollout, not this image
        ("eth", "alchemy"),  # unsupported provider
        ("eth", "onfinality"),  # dropped provider (deep proofs unservable)
        ("ethereum", "drpc"),  # alias, not the canonical slug
        ("ETH", "drpc"),  # case must be canonical lower
    ):
        try:
            validate_release_inputs(valid, chain, provider)
        except ValueError:
            continue
        raise AssertionError(f"expected ValueError for off-allowlist {(chain, provider)!r}")


def test_validate_accepts_valid_pinned_ref():
    # A real digest-pinned ref with canonical chain/provider passes.
    validate_release_inputs(
        "ghcr.io/taostat/bm-tee-miner@sha256:" + "a" * 64, "eth", "drpc"
    )
    # A tag before the digest is allowed (repo[:tag]@sha256:...).
    validate_release_inputs(
        "ghcr.io/taostat/bm-tee-miner:v1@sha256:" + "0123456789abcdef" * 4,
        "eth",
        "drpc",
    )
    # A host with an explicit port is a valid registry host.
    validate_release_inputs(
        "registry.example.com:5000/taostat/bm-tee-miner@sha256:" + "b" * 64,
        "eth",
        "drpc",
    )


def test_validate_rejects_bad_oci_reference():
    # The old permissive char class accepted these; the OCI grammar must
    # reject them.
    digest = "@sha256:" + "0" * 64
    for bad in (
        "ghcr.io/" + digest,  # empty repo
        "ghcr.io/Taostat/bm-tee-miner" + digest,  # uppercase repo component
        "ghcr.io/taostat/BM-TEE-MINER" + digest,  # uppercase repo component
        "x" + digest,  # bare single-component host (no dot, no port)
        "ghcr.io" + digest,  # host with no repo path
        "ghcr.io//x" + digest,  # empty path component
        "ghcr.io/-x" + digest,  # path component may not start with a separator
    ):
        try:
            validate_release_inputs(bad, "eth", "drpc")
        except ValueError:
            continue
        raise AssertionError(f"expected ValueError for invalid OCI ref {bad!r}")


def test_compute_compose_hash_validates_inputs():
    # The core hash-producing path validates too, not just the
    # CLI — a malformed digest raises rather than being silently baked into
    # a measured compose_hash. build_app_compose (the shared choke point)
    # enforces it, so both entry points are covered.
    for bad_ref in ("ghcr.io/x@sha256:deadbeef", "img@sha256:0", "@sha256:" + "0" * 64):
        try:
            compute_compose_hash(bad_ref, "eth", "drpc", _TEST_TEMPLATE, "#!/bin/sh\n")
        except ValueError:
            continue
        raise AssertionError(f"expected ValueError from compute_compose_hash for {bad_ref!r}")
    # build_app_compose validates its inputs directly as well.
    try:
        build_app_compose("img@sha256:0", "eth", "drpc", _TEST_TEMPLATE, "#!/bin/sh\n")
    except ValueError:
        pass
    else:
        raise AssertionError("expected ValueError from build_app_compose for a malformed digest")


def test_matches_golden_app_compose_when_present():
    # Byte-for-byte proof that this tool's canonical serialization matches
    # a real `phala prepare` output. Requires a real deploy artifact (a
    # bring-up dependency) — skipped offline until the fixture is dropped
    # in. Once present it runs in CI with no network.
    if not (GOLDEN_APP_COMPOSE.exists() and GOLDEN_COMPOSE_HASH.exists()):
        print("  (skip: golden app_compose fixture not present — bring-up dependency)", file=sys.stderr)
        return
    golden_obj = json.loads(GOLDEN_APP_COMPOSE.read_text(encoding="utf-8"))
    expected = GOLDEN_COMPOSE_HASH.read_text(encoding="utf-8").strip().lower()
    assert re.fullmatch(r"[0-9a-f]{64}", expected), expected
    got = hash_app_compose(golden_obj)
    assert got == expected, f"canonicalization drift: derived {got} != golden {expected}"


def _self_test() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    failed = 0
    for test in tests:
        try:
            test()
        except AssertionError as exc:
            failed += 1
            print(f"FAIL {test.__name__}: {exc}", file=sys.stderr)
        else:
            print(f"ok   {test.__name__}", file=sys.stderr)
    print(f"{len(tests) - failed}/{len(tests)} passed", file=sys.stderr)
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image-ref", help="digest-pinned image ref (ghcr.io/...@sha256:...)")
    parser.add_argument("--chain", default="eth", help="chain slug rendered into the compose")
    parser.add_argument(
        "--provider", default="drpc", help="provider slug rendered into the compose"
    )
    parser.add_argument(
        "--compose-template",
        type=Path,
        default=DEFAULT_COMPOSE_TEMPLATE,
        help="path to the compose template (default: tee/dstack/docker-compose.yaml)",
    )
    parser.add_argument(
        "--prelaunch-script",
        type=Path,
        default=DEFAULT_PRELAUNCH_SCRIPT,
        help="path to the pre-launch script (default: tee/dstack/prelaunch.sh)",
    )
    parser.add_argument(
        "--render",
        action="store_true",
        help="print the rendered docker-compose.yaml instead of the hash",
    )
    parser.add_argument("--self-test", action="store_true", help="run the built-in unit tests")
    args = parser.parse_args()

    if args.self_test:
        return _self_test()
    if not args.image_ref:
        parser.error("--image-ref is required (or use --self-test)")
    # Strictly validate the digest and reject newlines / off-allowlist
    # chain/provider before any of these strings are rendered into YAML.
    try:
        validate_release_inputs(args.image_ref, args.chain, args.provider)
    except ValueError as exc:
        parser.error(str(exc))

    template = args.compose_template.read_text(encoding="utf-8")
    prelaunch = args.prelaunch_script.read_text(encoding="utf-8")
    if args.render:
        print(render_compose(template, args.image_ref, args.chain, args.provider), end="")
    else:
        print(
            compute_compose_hash(args.image_ref, args.chain, args.provider, template, prelaunch)
        )
        # Remind the operator that the deploy MUST pin the OS image: the
        # boot measurements are bound to it, not to compose_hash, so the
        # hash on stdout is only half of what the registry approves.
        print(f"# deploy MUST pin: {deploy_image_pin_hint()}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
