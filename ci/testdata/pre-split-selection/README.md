# Historical selector contract fixture

These inputs preserve the pre-split selector assertions, including external
backend affinity. They are frozen data from the commit and exact source hashes
in `source-lineage.json`. The DAG and cell plan contain every original row and
only the fields consumed by their unchanged selector loaders. The footprint and
shard objects are unchanged.

`ci/select-tests.rs --self-test` checks the current generated core inputs first.
It then runs every original assertion against this fixture. Passing the fixture
provides no current external build, SDK, client, or guest qualification. Actual
SaBRe/e9 and DynamoRIO commands remain registered in their separate extension
`ci/retained-ci-catalog.json` files. Unknown paths and stale references still
force the complete current core suite.
