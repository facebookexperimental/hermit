#!/usr/bin/env python3
"""Pin-bump report for the private-static runtime source rows.

The private-static producer admits the runtime sources whose rows it derives
from the chosen Hermit head and the Reverie commit its gitlink records
(owned/runtime_cohort.py). A Reverie re-pin therefore changes the admitted
rows without any edit here, and the review attention those rows used to get
from a hand-updated literal must come from the pin-bump pull request instead.

Every Hermit pull request that moves the Reverie pin pastes this report into
its description. For each derived Reverie row whose content differs between
OLD and NEW it prints the path, both byte counts and sha256 digests, and a
short diff stat. A changed reverie-inguest/src/trap.rs also gets the x86_64
and aarch64 gate-block digests; a changed x86_64 block is refused by the build
until its reviewed digest is updated, so the report says so.

The report is empty (a header and no "R " lines) when no derived row changed.
Objects are read from --reverie-git (default: the reverie submodule of the
Hermit checkout this script lives in); a missing revision or row is refused
by name.

usage: pin_bump_report.py [--reverie-git DIR] OLD_REVERIE_REV NEW_REVERIE_REV
"""
import argparse
import importlib.util
from pathlib import Path
import sys

sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--reverie-git', default=str(D.parents[1] / 'reverie'),
                        help='Git directory or checkout holding both Reverie revisions')
    parser.add_argument('old', help='40-hex Reverie commit the base Hermit tree pins')
    parser.add_argument('new', help='40-hex Reverie commit the pin-bump head pins')
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location('pin_bump_runtime_cohort', D / 'owned/runtime_cohort.py')
    cohort = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cohort)
    try:
        rows = cohort.pin_bump_report(Path(args.reverie_git), args.old, args.new)
    except RuntimeError as refusal:
        print('pin_bump_report: ' + str(refusal), file=sys.stderr)
        return 1
    sys.stdout.write(cohort.format_pin_bump_report(args.old, args.new, rows))
    return 0


if __name__ == '__main__':
    sys.exit(main())
