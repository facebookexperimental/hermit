"""Hermit e2e manifest cells as Buck tests, run by Tpx locally or on Meta RE.

One test per cell of ci/expected-e2e-plan.json. Each runs `test-harness run` for exactly
that cell (cell.sh) and reports one Tpx HPHP-JSON result with the cell's evidence (the
harness rows, verify report and both DETLOGs) as Tpx artifacts. Tpx owns retries: cells
pass --no-retry to the harness, so `buck2 test ... -- --retry 1` retries a failed cell
and the ingester keeps "passed only on rerun" red.

Routing (per cell, first match wins; see _route):
  local: kvm backend, privileged lane, a host-capability requirement (every kvm cell names
         kvm, and every liteinst and in-guest-trap cell names cpuid-faulting, which the
         in-guest LiteInst runtime needs and RE workers may lack), a `requires` tool RE
         workers lack, the LOCAL_TESTS deny-list, a test measured to fail only on RE
         (re_exclusions.json), a PMU-armed cell under -c hermit_e2e.pmu_on_re=false, or
         the ptrace verify cell of a test whose kvm, liteinst or in-guest-trap verify cell
         runs locally (its parity reference).
         A sabre or dbt verify cell also runs locally whenever its test's ptrace verify
         cell (its parity reference) does, so the pair always shares a route.
  re:    everything else.
`-c hermit_e2e.routing=local` runs every cell locally (the buck-local test mode).

Container (per local cell; see _container): a privileged-lane cell and a pinned-root-only
cell run inside ci/hermetic/run-in-pinned-root.sh, the privileged podman
container the cargo flow runs every e2e node in. Other local cells run on the host.

RE-routed cells support Buck's test execution caching: when the bundle (hermit, harness,
fixtures, sources) and the cell are unchanged, Buck answers from the RE action cache
instead of re-running. Local executions are never cached by Buck, and `--stress-runs`
disables caching.
"""

RE_PLATFORM = {"platform": "linux-remote-execution"}

# Guest tools a cell may run that RE workers (CentOS Stream 9) do not have, measured
# 2026-10-01/02.
RE_MISSING_TOOLS = ["clang", "cmake", "gdb", "git", "lua5.4", "node", "ruby", "rustc", "tclsh"]

# Cells that must run locally for reasons `requires` does not express.
LOCAL_TESTS = {
    "c-programs/environment-and-workdir": "asserts a fresh tmpfs workdir at /test, which RE cannot create",
    "compat/cargo": "runs the host rustup toolchain through compat fixtures",
    "compat/clang": "runs clang, missing on RE, without declaring it",
    "compat/cmake": "runs cmake, missing on RE, without declaring it",
    "compat/git": "runs git, missing on RE, without declaring it",
    "compat/lua": "runs /usr/bin/lua, missing on RE, without declaring it",
    "compat/ruby": "runs /usr/bin/ruby, missing on RE, without declaring it",
    "compat/tcl": "runs tclsh, missing on RE, without declaring it",
}

# Cells that pass only inside the pinned-root container the cargo flow uses: neither a
# buck-local host nor an RE worker provides that identity, so they run locally inside
# that container (_container).
PINNED_ROOT_ONLY = {
    "c-programs/environment-and-workdir/custom": "asserts HOSTNAME=hermetic-container.local, the pinned-root PATH and /results paths, which only the pinned-root container provides",
    "c-programs/environment-and-workdir/verify": "asserts a fresh tmpfs workdir at /test, which cell.sh gives only pinned-root cells (elsewhere the workdir is the bound /tmp/test)",
}

# ptrace/liteinst/in-guest-trap cells that run with --max-timeslice=disabled and so never arm the PMU.
PMU_FREE_CELLS = [
    "c-programs/io-uring-fallback/custom@ptrace",
    "system-utils/clock-determinism/custom@liteinst",
    "system-utils/clock-determinism/custom@ptrace",
]

# Parity candidate backends whose verify cell is routed on its own; when it runs locally,
# its test's ptrace verify cell (the parity reference) follows it there. sabre and dbt
# verify cells instead follow their reference (hermit_e2e_cells).
PARITY_LEADING_BACKENDS = ["in-guest-trap", "kvm", "liteinst"]

def cell_id(cell):
    return "{}/{}@{}".format(cell["test"], cell["mode"], cell["backend"])

def cell_slug(cell):
    # The harness's own artifact slug: unique over the plan, at most 63 characters.
    return "{}-{}-{}".format(cell["test"].replace("/", "-"), cell["mode"], cell["backend"])

def pmu_armed(cell):
    return cell["backend"] in ["ptrace", "liteinst", "in-guest-trap"] and cell["category"] != "compat" and cell_id(cell) not in PMU_FREE_CELLS

def _route(cell, pmu_on_re, re_exclusions, local_candidates):
    """Returns (where, reason): where is "local" or "re"."""
    if cell["backend"] == "kvm":
        return ("local", "kvm backend: RE workers have no /dev/kvm")
    if cell["lane"] == "privileged":
        return ("local", "privileged lane")
    if cell.get("requires_host_capabilities"):
        return ("local", "needs host capability " + ",".join(cell["requires_host_capabilities"]))
    missing = [t for t in cell.get("requires", []) if t in RE_MISSING_TOOLS]
    if missing:
        return ("local", "requires " + ",".join(missing) + ", missing on RE")
    pinned = PINNED_ROOT_ONLY.get("{}/{}".format(cell["test"], cell["mode"]))
    if pinned:
        return ("local", "pinned-root-only: " + pinned)
    if cell["test"] in LOCAL_TESTS:
        return ("local", LOCAL_TESTS[cell["test"]])
    if cell["test"] in re_exclusions:
        return ("local", "measured: " + re_exclusions[cell["test"]]["reason"])
    if pmu_armed(cell) and not pmu_on_re:
        return ("local", "arms the PMU and -c hermit_e2e.pmu_on_re=false")
    if cell["backend"] == "ptrace" and cell["mode"] == "verify" and cell["test"] in local_candidates:
        # Backend parity credits a candidate verify cell against this test's ptrace verify
        # cell only when both ran on one route (ci/manifest-plan/src/parity.rs shares_route),
        # and these candidates run locally.
        backends = local_candidates[cell["test"]]
        if len(backends) == 1:
            return ("local", "parity reference of a {} verify cell, which runs locally".format(backends[0]))
        return ("local", "parity reference of the {} verify cells, which run locally".format(", ".join(backends)))
    return ("re", "")

def _container(cell, where):
    """Returns (container, reason) for a cell routed `where`: container is "pinned-root" or ""."""
    if where != "local":
        return ("", "")
    if cell["lane"] == "privileged":
        return ("pinned-root", "privileged lane: the cargo flow runs it in the privileged pinned-root container")
    pinned = PINNED_ROOT_ONLY.get("{}/{}".format(cell["test"], cell["mode"]))
    if pinned:
        return ("pinned-root", "pinned-root-only: " + pinned)
    return ("", "")

def _cell_test_impl(ctx):
    cmd = cmd_args(ctx.attrs.runner, ctx.attrs.args)
    remote = ctx.attrs.route == "re"
    executor = CommandExecutorConfig(
        local_enabled = not remote,
        remote_enabled = remote,
        remote_execution_properties = RE_PLATFORM,
        remote_execution_use_case = ctx.attrs.re_use_case,
        # Lookups only: Buck never uploads test results it executed locally.
        remote_cache_enabled = remote,
    )
    env = dict(ctx.attrs.env)
    env["HERMIT_E2E_BUNDLE"] = ctx.attrs.bundle[DefaultInfo].default_outputs[0]
    return [
        DefaultInfo(),
        RunInfo(args = cmd),
        ExternalRunnerTestInfo(
            type = "json",
            command = [cmd],
            env = env,
            labels = ctx.attrs.labels,
            default_executor = executor,
            run_from_project_root = True,
            use_project_relative_paths = True,
            supports_test_execution_caching = remote,
        ),
    ]

hermit_e2e_cell_test = rule(
    impl = _cell_test_impl,
    attrs = {
        "args": attrs.list(attrs.string()),
        "bundle": attrs.dep(),
        "env": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "labels": attrs.list(attrs.string(), default = []),
        "re_use_case": attrs.string(default = "fbcode_re_tests"),
        "route": attrs.enum(["local", "re"]),
        "runner": attrs.source(),
    },
)

def _bundle_impl(ctx):
    """The cell runner's input tree: hermit/{hermit,install}, bin/, src/, build/, run-state/."""
    hermit = ctx.attrs.hermit[DefaultInfo].default_outputs[0]
    entries = {
        "bin": ctx.attrs.harness_bin,
        "build": ctx.attrs.fixtures,
        "hermit/hermit": hermit,
        "hermit/install": ctx.attrs.install,
        "run-state": ctx.attrs.run_state,
        "src": ctx.attrs.src,
        "SOURCE_SHA": ctx.attrs.source_sha,
    }
    return [DefaultInfo(default_output = ctx.actions.copied_dir("bundle", entries))]

hermit_e2e_bundle = rule(
    impl = _bundle_impl,
    attrs = {
        "fixtures": attrs.source(allow_directory = True),
        "harness_bin": attrs.source(allow_directory = True),
        "hermit": attrs.dep(),
        "install": attrs.source(allow_directory = True),
        "run_state": attrs.source(allow_directory = True),
        "source_sha": attrs.source(),
        "src": attrs.source(allow_directory = True),
    },
)

def hermit_e2e_cells(plan, re_exclusions, bundle = ":bundle", runner = "cell.sh", labels = []):
    """One hermit_e2e_cell_test per plan cell, plus test suites all / re / local.

    `labels` are added to every cell's test, e.g. a consumer's CI scheduling labels.
    """
    routing = read_root_config("hermit_e2e", "routing", "hybrid")
    if routing not in ["hybrid", "local"]:
        fail("-c hermit_e2e.routing must be hybrid or local, got " + routing)
    pmu_on_re = read_root_config("hermit_e2e", "pmu_on_re", "true") == "true"
    # -c hermit_e2e.nonce=X gives every cell a fresh action digest: a cache-busting rerun.
    nonce = read_root_config("hermit_e2e", "nonce", "")
    # -c hermit_e2e.epoch=E gives every cell HERMIT_EPOCH=E. Backend parity compares a ptrace
    # reference with its candidates only when they ran under one guest epoch, and each cell is
    # its own harness process, which otherwise samples its own start time. validate-node sets
    # it from the checked-out commit's committer time.
    epoch = read_root_config("hermit_e2e", "epoch", "")
    if plan["schema"] != 1:
        fail("expected-e2e-plan.json schema must be 1")
    by_route = {"local": [], "re": []}
    # Tests whose verify cell on a backend that never follows its parity reference (kvm,
    # liteinst, in-guest-trap) routes locally, and by which backends. _route decides such a
    # cell without consulting this map, which only moves ptrace cells.
    local_candidates = {}
    for c in plan["cells"]:
        if c["mode"] == "verify" and c["backend"] in PARITY_LEADING_BACKENDS and _route(c, pmu_on_re, re_exclusions["tests"], {})[0] == "local":
            local_candidates[c["test"]] = sorted(local_candidates.get(c["test"], []) + [c["backend"]])
    references = {c["test"]: c for c in plan["cells"] if c["backend"] == "ptrace" and c["mode"] == "verify"}
    for cell in plan["cells"]:
        where, reason = _route(cell, pmu_on_re, re_exclusions["tests"], local_candidates)
        reference = references.get(cell["test"]) if cell["backend"] in ["sabre", "dbt"] and cell["mode"] == "verify" else None
        if where == "re" and reference:
            # Backend parity credits this cell against its test's ptrace verify cell only
            # when both ran on one route (ci/manifest-plan/src/parity.rs shares_route), so
            # it follows that reference's whole routing decision, under every config. (The
            # DBT adapter takes the user namespace its --bind mounts need, so it runs anywhere.)
            reference_where, reference_reason = _route(reference, pmu_on_re, re_exclusions["tests"], local_candidates)
            if reference_where == "local":
                where, reason = "local", "its parity reference runs locally: " + reference_reason
        if routing == "local":
            where = "local"
        container, container_reason = _container(cell, where)
        name = cell_slug(cell)
        env = {
            "HERMIT_E2E_CONTAINER": container,
            "HERMIT_E2E_CONTAINER_REASON": container_reason,
            "HERMIT_E2E_ROUTE": where,
            "HERMIT_E2E_ROUTE_REASON": reason,
        }
        if nonce:
            env["HERMIT_E2E_NONCE"] = nonce
        if epoch:
            env["HERMIT_EPOCH"] = epoch
        hermit_e2e_cell_test(
            name = name,
            runner = runner,
            args = [cell["test"], cell["mode"], cell["backend"]],
            bundle = bundle,
            env = env,
            labels = [
                "tpx-enable-artifact-reporting",
                "hermit_e2e",
                "hermit_e2e_route_" + where,
                "hermit_e2e_backend_" + cell["backend"],
            ]
            + (["hermit_e2e_container_" + container.replace("-", "_")] if container else [])
            + labels,
            route = where,
        )
        by_route[where].append(":" + name)
    native.test_suite(name = "all", tests = by_route["local"] + by_route["re"])
    native.test_suite(name = "re", tests = by_route["re"])
    native.test_suite(name = "local", tests = by_route["local"])
