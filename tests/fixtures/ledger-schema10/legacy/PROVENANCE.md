# Legacy schema-10 fixtures

These eight JSON/JSONL files are byte-identical copies of the `ordinary-only`
and `reference-diverged` producer exports in the parent dev-hermit repository at
`45fa84a89d773af4816afc02354adbb62952553f`, under
`ci-hub/validate/tests/fixtures/schema10/`.

Their source is Hermit commit `73146a90f8f1dd468471ffab448b6b7d71f33bf6`,
tree `0dff6f5354eabd11f8b539161f0d5b40c9ce0467`. `SHA256SUMS.json` records
the parent manifest digest and each original path, Git blob, byte count and
SHA-256 digest. Copy readback confirmed exact equality with both that manifest
and the committed parent objects.

These are **synthetic parser and artifact-verifier controls**, not guest runs,
backend coverage or evidence of current runtime success. Their fabricated paths
and reports are deliberate. The source exporter historically ran the shared
verifier; copying these bytes runs no test and grants no validation receipt.

Preserve the exact legacy bytes, including absent binding-contract markers and
selected-attempt fields. New bound-format controls must be separate producer
exports. Do not edit these old reports or relax their comparison fields to make
the new reader accept them.
