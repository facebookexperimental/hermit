# Series-snapshot fixture

The scorecard self-test's regression tier resolves `series/` and
`alternate-series/` as committed series sources: it checks the recorded
commit, tree, shard population, and that equivalent spellings and symlinks
reduce to the same source, without building a Git repository. The two
`series/` shards hold one row, the second with leading whitespace, so they
must collapse to that row. The rows carry fixed tree identities.

The self-test refuses a snapshot whose worktree differs from `HEAD`, so commit
any change to these files before running it.
