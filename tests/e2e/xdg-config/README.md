# E2E XDG configuration seed

`target/debug/test-harness` copies this directory into a fresh repo-local
`target/e2e/runs/<run>/.../xdg-config` directory for every test cell and sets
`XDG_CONFIG_HOME` to that copy. A `verify` cell on a backend that accepts
`--bind` (every backend except dbt) sees the copy at `/tmp/e2e/xdg-config`
instead, so each backend's guest names the same path. The checked-in Git
configuration provides a deterministic fixture identity. Tests never read or
write the invoking user's `~/.config` directory.
