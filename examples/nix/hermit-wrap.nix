# hermit-wrap.nix — the "execBuilder seam": run a Nix derivation's whole builder
# process tree under `hermit run`, with no patch to nix and no patch to nixpkgs.
#
# Background: nixpkgs `stdenv.mkDerivation` builds by exec'ing
#     realBuilder = stdenv.shell            (the binary that is exec'd)
#     args        = ["-e" source-stdenv.sh default-builder.sh]
# The user-facing `builder` *attribute* is the phase script, NOT the exec'd
# binary; `realBuilder` is the binary. Overriding `realBuilder` therefore puts
# hermit around unpack -> patch -> configure -> build -> install -> fixup while
# nix keeps evaluation, dependency ordering, output registration and comparison.
#
# The original builder is read off the ALREADY-EVALUATED derivation
# (`drv.drvAttrs.builder`), so packages with a non-default builder (or a
# different stdenv) wrap correctly. `hermitize` / `hermitizeIfNeeded` /
# `overlayFor` let a nixpkgs consumer opt in ONE package at a time.
#
# Because `realBuilder` is part of the input-addressed derivation, wrapping
# changes the derivation identity and hence the output path. Compare
# wrapped-vs-wrapped for reproducibility; compare wrapped-vs-native separately
# for semantic parity.

{ pkgs ? import <nixpkgs> { }
, # Absolute HOST path to the hermit binary (not a store path). The builder must
  # see the host filesystem, i.e. nix runs with `sandbox = false`.
  hermit
, # Pin --epoch: since hermit 402ba9737 an omitted epoch is one host wall-clock
  # sample per run, so every timestamp would differ.
  hermitArgs ? [ "run" "--epoch=2026-01-01T00:00:00Z" ]
, # `setarch -R` (ADDR_NO_RANDOMIZE) pins ASLR at the host level.
  setarch ? "/usr/bin/setarch"
, # Launch hermit through a host-side wrapper script, for example one that
  # bounds the run. Such a script needs a host PATH, while nix gives the builder
  # PATH=/path-not-set; so the launcher runs with PATH=/usr/bin:/bin and the guest
  # builder is re-exec'd through /usr/bin/env with nix's ORIGINAL PATH restored,
  # leaving the builder's environment as it is without the launcher.
  hostLauncher ? null
, # Show the guest ONE build-directory path on every build. nix 2.35 names the
  # build directory /nix/var/nix/builds/nix-<pid>-<random u32>, and the decimal
  # random part is 8, 9 or 10 digits long. The guest sees that path in PWD,
  # TMPDIR and NIX_BUILD_TOP, so its length changes how many instructions the
  # builder executes and therefore hermit's virtual clock: two otherwise
  # identical builds then differ (measured 2026-10-02: the 17/3 split of the
  # nondet-demo-fast probe). When set, the build directory is bind-mounted at
  # /tmp/build on hermit's private /tmp and the builder starts there, which is
  # what nix's own sandbox does with /build. Requires hermitArgs WITHOUT
  # --tmp=/tmp, so that guest /tmp is hermit's private directory.
  canonicalBuildDir ? true
, useSetarch ? false
}:

let
  inherit (pkgs) lib stdenv;

  # Bake the arch at eval time: `uname` is not on the builder's PATH.
  arch = stdenv.hostPlatform.uname.processor; # e.g. "x86_64"
  # With canonicalBuildDir the host side exports the canonical values BEFORE
  # hermit starts: the first guest process must already see the same
  # environment on every build, because even an `env VAR=...` running inside the
  # guest spends a length-dependent number of branches on the random path.
  # Host tools get TMPDIR=/tmp (the guest-side /tmp/build does not exist on the
  # host) and the guest prefix restores TMPDIR=/tmp/build.
  hostTmp = lib.optionalString canonicalBuildDir "TMPDIR=/tmp ";
  hermitCmd =
    if hostLauncher == null then lib.optionalString canonicalBuildDir "/usr/bin/env ${hostTmp}" + lib.escapeShellArgs ([ hermit ] ++ hermitArgs)
    else "/usr/bin/env PATH=/usr/bin:/bin ${hostTmp}" + lib.escapeShellArgs ([ "/bin/bash" hostLauncher hermit ] ++ hermitArgs);
  guestPrefix = lib.optionalString (hostLauncher != null) ''/usr/bin/env "PATH=$PATH" ''
    + lib.optionalString canonicalBuildDir "/usr/bin/env TMPDIR=/tmp/build PWD=/tmp/build ";
  canonicalPre = lib.optionalString canonicalBuildDir ''
    build_dir="$NIX_BUILD_TOP"
    export NIX_BUILD_TOP=/tmp/build TMPDIR=/tmp/build TEMPDIR=/tmp/build TMP=/tmp/build TEMP=/tmp/build
    cd / && export PWD=/ && unset OLDPWD
  '';
  canonicalArgs = lib.optionalString canonicalBuildDir
    ''--bind "$build_dir:/tmp/build" --workdir /tmp/build '';
  prefix = lib.optionalString useSetarch "${lib.escapeShellArg setarch} ${arch} -R ";

  # A store-resident shell script that re-execs the derivation's ORIGINAL
  # builder under hermit, forwarding the original argv unchanged.
  mkWrapper = origBuilder:
    pkgs.writeShellScript "hermit-exec-builder" (canonicalPre + ''
      exec ${prefix}${hermitCmd} ${canonicalArgs}-- ${guestPrefix}${lib.escapeShellArg origBuilder} "$@"
    '');

  # ---- the public API -------------------------------------------------------

  # hermitize : derivation -> derivation
  # Run this one derivation's builder under hermit. Idempotent.
  hermitize = drv:
    if (drv.passthru or { }) ? hermitWrapped then drv
    else drv.overrideAttrs (old: {
      realBuilder = mkWrapper drv.drvAttrs.builder;
      passthru = (old.passthru or { }) // { hermitWrapped = true; };
    });

  # hermitizeIfNeeded : derivation -> derivation
  # Opt-in by CONVENTION: a package declares `passthru.needsHermit = true;`
  # (in nixpkgs, or in a small overlay next to it) and this helper is a no-op
  # for every other package. That is the "enable hermit for ONLY the builds
  # that need it" knob: no nix patch, no nixpkgs fork.
  hermitizeIfNeeded = drv:
    if (drv.passthru or { }).needsHermit or false then hermitize drv else drv;

  # overlay : the same thing as a nixpkgs overlay, applied to a NAMED set of
  # packages. Usage:
  #   import <nixpkgs> { overlays = [ ((import ./hermit-wrap.nix {}).overlayFor [ "unrar" "zsh" ]) ]; }
  overlayFor = names: final: prev:
    lib.genAttrs names (n: hermitize prev.${n});

  # overlayNeedsHermit : honours `passthru.needsHermit` across an explicit list.
  overlayNeedsHermit = names: final: prev:
    lib.genAttrs names (n: hermitizeIfNeeded prev.${n});
in
{
  inherit hermitize hermitizeIfNeeded overlayFor overlayNeedsHermit mkWrapper;
}
