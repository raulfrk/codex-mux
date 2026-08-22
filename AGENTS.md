# Codex Mux repository instructions

Before declaring any release candidate ready, run the complete Rust test suite and the authenticated installed-artifact journeys against the exact binary that will be released.

The authenticated gate is mandatory and fail-closed. Build inside the same
pinned image, with the same target and reproducibility environment as release
CI, then package and extract the candidate. The checked-in digest is verified
again by the release workflow, so this exact authenticated x86 binary is the
one published for that target:

```sh
docker run --rm -v "$PWD:/work" -w /work \
  -e CARGO_INCREMENTAL=0 -e SOURCE_DATE_EPOCH=0 \
  rust:1.85.0-bullseye@sha256:0fdb1727a6c81e0811df9d67bd6defa9a4a07d2a44373ab0a017aedad9fcba3f \
  bash -lc 'rustup target add x86_64-unknown-linux-gnu --toolchain 1.85.0 && cargo +1.85.0 build --locked --release --target x86_64-unknown-linux-gnu'
scripts/package-release.sh 0.11.0 x86_64-unknown-linux-gnu \
  target/x86_64-unknown-linux-gnu/release/codex-mux dist
mkdir -p target/authenticated-candidate
tar -xzf dist/codex-mux-0.11.0-x86_64-unknown-linux-gnu.tar.gz \
  -C target/authenticated-candidate
candidate=$(realpath target/authenticated-candidate/codex-mux-0.11.0-x86_64-unknown-linux-gnu/codex-mux)
digest=$(sha256sum "$candidate" | cut -d' ' -f1)
printf '%s  %s\n' "$digest" target/x86_64-unknown-linux-gnu/release/codex-mux \
  > release-candidate-x86_64.sha256
CODEX_MUX_RUN_AUTHENTICATED_JOURNEYS=1 \
CODEX_MUX_AUTHENTICATED_CODEX=/absolute/path/to/codex \
CODEX_MUX_CANDIDATE_BINARY="$candidate" \
CODEX_MUX_CANDIDATE_SHA256="$digest" \
cargo test --test authenticated_journeys -- --ignored --nocapture
```

Do not substitute mocks, prompt-only checks, or an older installed binary. Record the tested Git commit and SHA-256 digest of the candidate binary in the journey result. If credentials, tmux, the configured Codex executable, or another prerequisite is unavailable, report the release gate as blocked; do not skip it and call the release ready.

Run the complete suite with a fresh evidence log and the packaged journeys
fail-closed against that candidate. Then validate the evidence log against the
production action inventory:

```sh
mkdir -p target/release-e2e-tmp
evidence=$(realpath target/release-journeys.log)
install -m 600 /dev/null "$evidence"
CODEX_MUX_REQUIRE_PACKAGED_E2E=1 \
CODEX_MUX_E2E_BINARY="$candidate" \
CODEX_MUX_JOURNEY_EVIDENCE="$evidence" \
TMPDIR=$(realpath target/release-e2e-tmp) \
cargo test --locked --all-targets --all-features -- --test-threads=1
CODEX_MUX_VALIDATE_JOURNEY_EVIDENCE=1 \
CODEX_MUX_JOURNEY_EVIDENCE="$evidence" cargo test --test journey_catalog
```

Shell scripts may prepare an outer packaging sandbox, but journey behavior and
assertions belong in Rust.
