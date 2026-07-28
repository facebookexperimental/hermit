# E2E XDG configuration seed

`ci/test_harness.sh` copies this directory into a fresh repo-local
`target/e2e/runs/<run>/.../xdg-config` directory for every test cell and sets
`XDG_CONFIG_HOME` to that copy. The checked-in Git configuration provides a
deterministic fixture identity. Tests never read or write the invoking user's
`~/.config` directory.
