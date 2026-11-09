# Coarse-selection fixture (pre-split graph)

The frozen pre-split validation graph that `ci/select-tests.rs`
(`coarse_selection_bracket`) and `scripts/validate.rs`
(`historical_dbt_selective_subset_bracket`) check the selector and planner
against, now that the live graph has no DynamoRIO, SaBRe or e9patch nodes.

`validate.json` is a projection of `ci/dag/validate.json` at Hermit
`7599a549e303` (3,051,891 bytes, sha256
`1ea6c4b531405ed285727f933f4f22e402bfd1cc58e8df90bfdb1771ca2e98e8`): every
step's `description` and `desc` are emptied and its `result_manifests` list is
cleared. Every other field, every step and the step order are unchanged. Both
brackets give identical results on the original and on the projection, and
removing one step from the projection fails them.

The other three files are unprojected copies from the same commit.
