# Neutral engine artifact producer

`artifact.py install` installs the actual Cargo-emitted engine DSO and this
package's matching ordinary and native C headers into a new directory. `artifact.py check` rechecks
that installation. These commands neither build nor run a guest. The producer
has no SaBRe, e9patch or DynamoRIO dependency, setting, discovery rule or mode.

The build caller supplies the normal project lease/private Cargo home,
configured job width, budget wrapper, one target directory and bounded process
cleanup. It retains the actual Cargo exit status, resource record, JSON event
stream and `cargo metadata --locked --offline --format-version 1` output. The
engine build selection is exactly:

```text
ci/run-with-reverie-dbt-budget.sh cargo build --locked --offline --release -p hermit-inguest-engine --lib --message-format=json-render-diagnostics
```

This is an interface description, not authorization to launch an unbounded
shell command. The normal runner must capture stdout directly as the Cargo
event file and preserve failure status before inspecting output. There is no
pipeline that substitutes a reader's exit status for Cargo's status.

After that actual successful build, invoke `python3 inguest-engine/artifact.py
install` with all of these required arguments:

| Argument | Required input |
| --- | --- |
| `--hermit-revision` | The current committed 40-character core Hermit HEAD |
| `--reverie-revision` | Its exact Reverie gitlink and all three engine dependency revisions |
| `--cargo-events` | The completed normal build's raw Cargo JSON stream |
| `--cargo-metadata` | Completed locked/offline metadata for that source and target |
| `--target-dir` | The same declared target directory used by both Cargo commands |
| `--destination` | A new absent installation directory |

Installation requires committed tracked package sources, their unchanged
worktree, exactly one emitted engine cdylib unit, one same-workspace Detcore,
and one matching compiled revision of each shared Reverie runtime package.
The feature list and actual profile come from that unit, including an explicitly
selected diagnostic observer if present. Backend packages refuse. The emitted
path must belong to the declared target; it is not found by searching for a
library filename. The ELF format check is deliberately only an architecture/
type check, not a substitute for link, symbol or loader qualification.

The schema-2 output contains `libhermit_inguest_engine.so`,
`hermit-engine-v1.h`, `hermit-engine-native-v1.h`, and a last-written
`engine-artifact.json`. Its artifact roles are exactly `engine`, `header`, and
`native_header`; each role binds the fixed filename, length and SHA-256.
Schema 1 has no native header binding and refuses in this producer successor.
Existing/partial destinations refuse.
Copies use held regular-file descriptors, compare metadata before and after,
and check the installed hashes. The record includes the exact H/R identities,
actual profile/features/compiled units and hashes of the lock, package, producer
and input records. It is consumed explicitly by extension producers; it is not
an assertion of runtime readiness or a substitute for the build's normal receipt.

The extension owns its native shim and loader build. It must compile the shim
against this installed header/DSO, install the exact artifacts together for the
declared `$ORIGIN` dependency, retain their whole source/build cohort, and prove
the actual `DT_NEEDED`, exported callbacks, constructors/relocations, allocator/
TLS ownership and genuine loader behavior. A different DSO per generic backend
does not satisfy the common-engine boundary. Private initial/successor/native
issuers and measured-source finalizers remain separate and unqualified here.

`tests/test_artifact.py` contains seven authored producer format/custody controls.
They do not compile or load their small file fixtures. The seven Rust
`loader_state` controls and the four core Reverie root-dispatch controls also
require actual execution at final signed source revisions. None has been run
as part of this source successor. The earlier six-control source/receipt stays
historical; the new control exercises the complete role/filename/hash check,
including missing, unknown, aliased and subsequently changed native headers.
