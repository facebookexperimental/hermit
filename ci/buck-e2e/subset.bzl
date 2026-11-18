"""Buck-only subsets of the e2e plan, applied by ci/buck-e2e/BUCK.

-c hermit_e2e.subset_lanes=L,... -c hermit_e2e.subset_backends=B,... and
-c hermit_e2e.subset_categories=C,... (all three, or none) keep only the cells in every
list, on top of what fb-only/e2e_cells.bzl keeps, for a Buck-only run of a subset whose
every input Buck builds: ci/buck-e2e/subsets/NAME.json, whose flags
ci/buck-e2e/subset.py buck-config prints, as scripts/validate.rs --buck-s0 passes them.
With none of the flags every function here returns its input unchanged. A lane, backend
or category the plan does not hold fails the load, as does a subset category the
fb-only file excludes or a subset that keeps no cell: a typo must not shrink the subset
silently. In a subset run that keeps kvm cells, kvm is a required host capability, so a
host without /dev/kvm fails those cells rather than skipping them.
"""

def _subset_list(key):
    value = read_root_config("hermit_e2e", "subset_" + key, "")
    return [v for v in value.split(",") if v]

_SUBSET = {key: _subset_list(key) for key in ["lanes", "backends", "categories"]}
_GIVEN = [key for key in _SUBSET if _SUBSET[key]]
if _GIVEN and len(_GIVEN) != 3:
    fail("give all of -c hermit_e2e.subset_lanes, subset_backends and subset_categories, or none; got " + ", ".join(_GIVEN))

_FIELDS = {"backends": "backend", "categories": "category", "lanes": "lane"}

def _check_known(plan):
    for key, field in _FIELDS.items():
        known = {cell[field]: True for cell in plan["cells"]}
        unknown = [v for v in _SUBSET[key] if v not in known]
        if unknown:
            fail("-c hermit_e2e.subset_{} names {} the plan does not hold".format(key, unknown))

def subset_cells(plan, cells):
    """The cells of `cells` (what fb-only/e2e_cells.bzl keeps) in every subset list."""
    if not _GIVEN:
        return cells
    _check_known(plan)
    kept = [cell for cell in cells if cell["lane"] in _SUBSET["lanes"] and cell["backend"] in _SUBSET["backends"] and cell["category"] in _SUBSET["categories"]]
    if not kept:
        fail("the -c hermit_e2e.subset_* flags keep no cell this build defines")
    return kept

def subset_categories(plan, categories):
    """The categories :fixtures builds: the subset's, each of which the fb-only file
    must keep (its empty list keeps every category)."""
    if not _GIVEN:
        return categories
    _check_known(plan)
    excluded = [c for c in _SUBSET["categories"] if categories and c not in categories]
    if excluded:
        fail("-c hermit_e2e.subset_categories names {}, which this build excludes".format(excluded))
    return sorted(_SUBSET["categories"])

def subset_excluded_backends(plan, excluded):
    """`excluded` (backend: reason) plus every plan backend outside the subset."""
    if not _GIVEN:
        return excluded
    _check_known(plan)
    out = dict(excluded)
    for cell in plan["cells"]:
        if cell["backend"] not in _SUBSET["backends"] and cell["backend"] not in out:
            out[cell["backend"]] = "outside the -c hermit_e2e.subset_backends subset"
    return out

def subset_required_host_caps(caps):
    """`caps`, plus kvm in a subset run that keeps kvm cells."""
    if _GIVEN and "kvm" in _SUBSET["backends"] and "kvm" not in caps:
        return caps + ["kvm"]
    return caps
