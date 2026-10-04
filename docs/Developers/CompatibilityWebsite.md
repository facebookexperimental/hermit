# Compatibility website publication

When activated, the `Docs` workflow rebuilds the compatibility website from the
existing public [`hermit_test_ledger`](https://github.com/rrnewton/hermit_test_ledger)
every day at 08:23 UTC. GitHub schedules can be delayed. Scheduled jobs remain
inactive unless the repository variable `COMPATIBILITY_NIGHTLY_ENABLED` is
exactly `true`; a skipped job is not a successful rebuild. A workflow file on a
feature branch does not activate it. Manual Docs publication remains available
while nightly rebuilding is inactive.

The same workflow publishes the complete documentation site: rustdoc, the
Hermetic Infra landing page, every retained compatibility build, and
`compatibility/latest/`. It has no pull-request or main-push trigger. All
publication runs are serialized and restricted to `rrnewton/hermit:main`.

## Source access

The maintained website builder is in the private `rrnewton/dev-hermit`
repository. Hermit's `GITHUB_TOKEN` can publish Hermit release assets and Pages,
but cannot read that other private repository. A repository administrator must:

1. Create an SSH deploy key dedicated to this website workflow. Add its **public**
   key to `rrnewton/dev-hermit` under **Settings → Deploy keys**, with write access
   disabled.
2. Put the corresponding **private** key in `rrnewton/hermit` under **Settings →
   Secrets and variables → Actions**, named `COMPATIBILITY_SOURCE_DEPLOY_KEY`.
   Do not put either private key material or a personal access token in Git,
   workflow output, release assets, or review reports.
3. Confirm the exact commits in `.github/compatibility-site-builder.json` are
   available in their named repositories. The parent commit's recorded Hermit
   gitlink must equal `hermit_commit`. Update these pins together when a reviewed
   builder or presentation change is delivered.
4. Run the manual fresh rebuild below and inspect its actual result and public
   byte verification. Then set the Actions repository variable
   `COMPATIBILITY_NIGHTLY_ENABLED` to `true` to enable the daily schedule. Leave
   it unset or `false` while source access or hosted execution is unavailable.

The checkout action uses the read-only key only to fetch the pinned parent
source and does not persist credentials. Subsequent setup initializes only
public Hermit, its Agent Utils dependency, and the public cell ledger. It does
not recursively materialize unrelated parent or backend dependencies. Missing
source access fails the job before rebuilding; an old archive is never reported
as a fresh ledger rebuild. The activation switch does not relax this check: an
enabled schedule or an explicit manual rebuild with missing access fails.

## Run and inspect a fresh rebuild

Once the workflow is on `main` and source access is configured:

```bash
gh workflow run docs.yml --repo rrnewton/hermit --ref main \
  -f rebuild_compatibility=true
```

This resolves the cell ledger's full immutable commit first, then fetches the
private parent's complete current history while checking out the reviewed
builder commit. The adapter fetches the exact captured public ledger object and
passes both captured data commits to the maintained builder with published-only
selection. The builder source, parent validation history, Hermit catalogue and
fresh Hermit main ancestry reference are recorded separately. A source pin does
not claim that all measurements ran at that source revision.

Fresh ledger rows do not bypass validation authority. A row produced by
`validate` needs the current canonical terminal and its exact portable plan,
cell and test artifacts from the existing public ledger. Missing current proof
stages no artifacts and retains the native reader's explicit Unavailable result;
a predecessor's proof cannot supply credit after a correction. Present malformed,
conflicting or tampered proof refuses the build. Pressure-test and repeat rows
retain their existing independent admission rules. A fetched ledger commit is
therefore not a claim that every newly published cell received comparison credit.
The reviewed reader does not run the standalone scorecard writer; that producer's
ordinary evidence-identity repair and authentic finalized-run publication remain
separate qualifications, not effects of rebuilding this website.

The build time, source commit dates, ledger snapshot commit, and measurement
timestamps are separate facts. Rebuilding does not run Hermit tests, create
new measurements, change a failed or missing result, or refresh measurement
timestamps. The existing typed reducer continues to determine the selected
cells, comparison credit, reference denominator and history.

The job validates the built website, creates an immutable checksum-bound
archive and append-only release registry, and requests a stable, non-draft data
release with `--latest=false`. It downloads and validates every retained archive
before deploying the full site. After deployment it compares nine ordinary
public URLs with the exact built manifest, page, CSS, JavaScript and browser
data. A stale or unreadable result makes the job fail. The uploaded receipt
records the exact source commits, archive identity and deployment result;
private source and credentials are not uploaded. Build success alone does not
establish public deployment.

The rebuild has a 3600-second inner deadline with 30 seconds of termination
grace inside a 61-minute step and a 120-minute complete-site job. Receipts record
observed runner CPU affinity, total/available memory and the largest individual
process RSS, plus accessible enclosing-cgroup limits and lifetime peak. Process
RSS is not aggregate job memory, and cgroup peak can include other steps. The
qualified local UI build reached its 8 GiB cgroup limit without an OOM kill.
Hosted capacity and successful execution must be observed on the actual runner,
not inferred from `ubuntu-latest`.
Failures before deployment keep the previous site. Public byte verification can
fail after Pages has been updated; that failure is reported without claiming an
automatic rollback.

## Publish a separately built website

A reviewed local build can reach Pages without the private-source credential.
Keep its new archive immutable, append its exact descriptor to a registry that
preserves every checked-in and already-served build, and upload the archive and
registry as stable release assets. Then dispatch the main workflow with the
registry asset ID and SHA-256:

```bash
gh workflow run docs.yml --repo rrnewton/hermit --ref main \
  -f compatibility_registry_asset=ASSET_ID \
  -f compatibility_registry_sha256=REGISTRY_SHA256
```

Leave `rebuild_compatibility` false for this handoff. Combining a fresh rebuild
with an explicit registry is refused. A manual Docs run with neither option
preserves the latest already-served registry, so rebuilding rustdoc cannot
silently roll the website back to an older checked-in snapshot.

Before this workflow change reaches `main`, the older manual Docs workflow has
no registry inputs. Its supported handoff is a reviewed registry-only source
change to `.github/compatibility-site-releases.json`, followed by the unchanged
manual main workflow. Never replace an old archive's bytes or reuse its content
identity for a new presentation.
