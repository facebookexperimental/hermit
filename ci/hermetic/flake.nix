{
  # Hermetic validate base image, stage 2 (OPT-IN; the default validate path is
  # unchanged).
  #
  # WHY NIX AND NOT JUST AN OCI DIGEST. A registry digest pins the ARTIFACT: if
  # the registry loses the blob, the image is gone and a validate run from a
  # month ago cannot be reproduced. A flake.lock pins the INPUTS, so the image
  # can be rebuilt from source at that lock even after third-party upgrades. A
  # receipt should name BOTH -- the digest for what ran, the lock for how to
  # rebuild it. See ci/hermetic/README.md for what a month-old rebuild actually
  # depends on staying available; the honest answer is not "nothing".
  #
  # WHAT THE IMAGE PINS, and why each is here rather than inherited from the host:
  #   * the Rust toolchain. `rust-toolchain.toml` says `channel = "nightly"`,
  #     which is a MOVING target -- the single largest source of "it built
  #     differently today". Pinned here to an exact dated nightly.
  #   * the C/C++ toolchain and native development libraries. Measured on this
  #     project: host `gcc` defaulting to `-march=x86-64-v2` put SSE4.1 into a
  #     static glibc that the emulator did not advertise, and a missing
  #     `libunwind-ptrace` broke a pinned build that had the compiler right.
  #   * every system executable a manifest runs AS A HERMIT GUEST. These are not
  #     build dependencies -- they are the program under test. A different
  #     `openssl` or `sqlite3` is a different guest.

  # PIN POLICY: current when set, refreshed deliberately, never left to rot.
  # A pin by revision + narHash is what gives the rebuild-from-source property;
  # being CURRENT is a separate property, and the two are not in conflict. The
  # first version of this file pinned nixpkgs at 2024-12-30 while rust-overlay
  # was current, and nobody noticed for the whole life of the change, because
  # the Rust toolchain comes from the overlay so the stale half was invisible.
  # At that revision `pkgs.rustc` was 1.77.2, which cannot build this repo at
  # all -- 30 of its crates are edition 2024, which needs 1.85 or newer. That
  # made "could we drop rust-overlay and use nixpkgs' own Rust?" look answered
  # in the negative when it was not.
  #
  # Do NOT replace these with a branch name to keep them fresh. A branch is not
  # reproducible. Re-pin to a new revision as a reviewed change instead.
  inputs = {
    # nixos-unstable @ 2026-08-23. `pkgs.rustc` here is 1.95.0 on the 26.05
    # release branch and 1.97.1 on unstable; unstable is taken because 1.97.1 is
    # the exact stable version the whole workspace has been shown to
    # `cargo check` clean against, so the drop-rust-overlay question can be
    # tested against this image rather than argued about.
    nixpkgs.url = "github:NixOS/nixpkgs/56c02bc00adcf003215cc4bd996d6efaf4cff188";
    # oxalica/rust-overlay @ 2026-08-24, which was already the tip when this
    # bump was made -- this input had not gone stale, only nixpkgs had.
    rust-overlay = {
      url = "github:oxalica/rust-overlay/ab450d47a3f906d19de1b332915bfc6e5b29c853";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };

      # Exact dated nightly. Bumping this is a reviewed change to the lock and
      # the digest together, never a silent drift.
      rustToolchain = pkgs.rust-bin.nightly."2026-07-29".default.override {
        extensions = [ "rust-src" "rustfmt" "clippy" ];
      };

      # These are the exact versions used by the portable CI contract. The
      # pinned nixpkgs revision carries older releases (rust-script 0.34.0 and
      # cargo-nextest 0.9.72), so using those attributes directly would make an
      # offline run green under tools different from the ones CI selected.
      pinnedRustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      rustScriptVersion = "0.36.0";
      rustScript = pinnedRustPlatform.buildRustPackage {
        pname = "rust-script";
        version = rustScriptVersion;
        src = pkgs.fetchFromGitHub {
          owner = "fornwall";
          repo = "rust-script";
          rev = rustScriptVersion;
          hash = "sha256-Bb8ULD2MmZiSW/Tx5vAAHv95OMJ0EdWgR+NFhBkTlDU=";
        };
        cargoHash = "sha256-kxnylNZ8FsaR2S1o/p7qtlaXsBLDNv2PsFye0rcf/+A=";
        doCheck = false;
      };
      cargoNextestVersion = "0.9.100";
      cargoNextest = pinnedRustPlatform.buildRustPackage {
        pname = "cargo-nextest";
        version = cargoNextestVersion;
        src = pkgs.fetchFromGitHub {
          owner = "nextest-rs";
          repo = "nextest";
          rev = "cargo-nextest-${cargoNextestVersion}";
          hash = "sha256-MbgX/n6TC5hz66gvRAc7A0xFWbF2Ec68gMxCgPFpeoQ=";
        };
        cargoHash = "sha256-jRBFjJB38JI9whFpImYlMx0znQj1+cdeu4Nc+nYc7OI=";
        cargoBuildFlags = [ "-p" "cargo-nextest" ];
        cargoTestFlags = [ "-p" "cargo-nextest" ];
        # The pinned nixpkgs cargo-auditable predates Rust edition 2024.
        auditable = false;
      };

      # Keep the existing coreutils output unchanged. Nixpkgs omits arch from
      # that output; enable its real GNU implementation in a separate output
      # and expose only arch below.
      # coreutils-full uses the normal pinned stdenv; overriding coreutils
      # directly would pull its earlier compiler-bootstrap inputs into this build.
      archCoreutils = (pkgs.coreutils-full.override { minimal = true; }).overrideAttrs (previous: {
        configureFlags = previous.configureFlags
          ++ [ "--enable-install-program=arch,kill,uptime" ];
      });
      requiredGuestPaths = pkgs.lib.filter (path: path != "")
        (pkgs.lib.splitString "\n" (builtins.readFile ./guest-paths.txt));

      # These command fixtures are required by the existing strict test loops.
      # Reference only their pinned executables, preserving every existing /bin
      # provider while retaining the packages' runtime closures in the image.
      commandFixtureTools = {
        iostat = "${pkgs.sysstat}/bin/iostat";
        lsmod = "${pkgs.kmod}/bin/lsmod";
        mpstat = "${pkgs.sysstat}/bin/mpstat";
        numactl = "${pkgs.numactl}/bin/numactl";
        numastat = "${pkgs.numactl}/bin/numastat";
        pidstat = "${pkgs.sysstat}/bin/pidstat";
        ss = "${pkgs.iproute2}/bin/ss";
      };
      commandFixtureLinks = pkgs.lib.concatStringsSep "\n" (pkgs.lib.mapAttrsToList
        (name: provider: ''
          if [ ! -f ${pkgs.lib.escapeShellArg provider} ] || [ ! -x ${pkgs.lib.escapeShellArg provider} ]; then
            echo ${pkgs.lib.escapeShellArg "missing executable pinned command fixture: ${name}"} >&2
            exit 1
          fi
          ln -sT ${pkgs.lib.escapeShellArg provider} ${pkgs.lib.escapeShellArg "bin/${name}"}
        '') commandFixtureTools);

      # Executables that the selected portable population runs as hermit guests.
      # This was re-audited mechanically from ci/expected-e2e-plan.json, each
      # selected manifest entry's requirements/program, and commands invoked by
      # shell fixtures. The second line supplies the previously missing lua, m4,
      # node, ssh-keygen, ruby, tclsh, uuidgen, mcookie, hexdump and ps.
      guestTools = with pkgs; [
        bash coreutils diffutils findutils gnugrep gnused gawk
        openssl zstd gnutar gzip xz jq sqlite git perl python3 redis
        lua5_4 gnum4 nodejs openssh ruby tcl util-linux procps
        # These outputs are not implied by their runtime libraries.
        bzip2.bin glibc.bin hostname
      ];

      # The CLI replay tests run GDB outside Hermit and execute Python commands.
      # Keep this test driver separate from guest and compiler dependencies.
      testTools = [ (pkgs.gdb.override { pythonSupport = true; }) ];

      # Native libraries need both their runtime and development outputs. A Nix
      # image does not populate FHS search paths, so the environment below makes
      # these exact pinned outputs visible to build scripts and the C compiler.
      nativeLibs = with pkgs; [ libunwind elfutils zlib openssl ];
      # ⚠️ unixtools.xxd IS A BUILD DEPENDENCY, NOT A CONVENIENCE. e9patch's Makefile
      # generates two C sources by running `xxd -i` over its loader binaries
      # (Makefile:73 and :79). Without it the build dies at `Error 127` -- command not
      # found -- AFTER gcc, cmake and the whole C++ toolchain have already succeeded,
      # so the failure reads as a compiler problem and is not one. Measured
      # 2026-08-27: every other tool the build needs was already present; xxd was the
      # single omission, and it is absent from run-split-validate.sh's required-tool
      # list too, which is why nothing caught it before the build did.
      buildTools = with pkgs; [
        rustToolchain gcc binutils gnumake cmake pkg-config rustScript cargoNextest
        unixtools.xxd
      ] ++ nativeLibs ++ map (package: package.dev) nativeLibs;
    in
    {
      packages.${system} = {
        image = pkgs.dockerTools.buildLayeredImage {
          name = "hermit-hermetic-validate";
          tag = "nix";
          # Fixed timestamp: a build whose output moves with the wall clock
          # cannot be checked for reproducibility.
          created = "1970-01-01T00:00:01Z";
          contents = guestTools ++ buildTools ++ testTools ++ [
            pkgs.dockerTools.binSh
            pkgs.dockerTools.usrBinEnv
            pkgs.dockerTools.caCertificates
          ];
          # A nix-built root is minimal: it has no FHS scratch directories at
          # all. Measured -- without this, `check-detcore-backend-abstraction.sh`
          # failed at `mktemp: failed to create directory via template
          # '/tmp/tmp.XXXXXXXXXX': No such file or directory`, and it failed in
          # the NEGATIVE CONTROL, so the lint reported itself untrustworthy
          # rather than passing vacuously. That is the good failure mode, but the
          # directories have to exist. Sticky-bit 1777 as on a normal system.
          extraCommands = ''
            mkdir -p tmp var/tmp usr/bin etc root lib64
            chmod 1777 tmp var/tmp
            chmod 0700 root
            printf 'root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\n' > etc/passwd
            printf 'root:x:0:\nnobody:x:65534:\n' > etc/group
            ln -s "${pkgs.glibc}/lib/ld-linux-x86-64.so.2" lib64/ld-linux-x86-64.so.2

            # Selected portable cells name these FHS paths literally. Nix
            # places their providers in /bin, while usrBinEnv creates only
            # /usr/bin/env; add exactly the audited compatibility paths.
            mkdir -p bin
            ln -s "${archCoreutils}/bin/arch" bin/arch
            ${commandFixtureLinks}
            for path in ${pkgs.lib.escapeShellArgs requiredGuestPaths}; do
              command="''${path##*/}"
              if [ "$command" = nodejs ]; then command=node; fi
              ln -s "/bin/$command" ".''${path}"
            done
          '';
          config = {
            Env = [
              "PATH=/bin:/usr/bin"
              "HERMIT_RUST_SCRIPT_VERSION=${rustScriptVersion}"
              "HERMIT_CARGO_NEXTEST_VERSION=${cargoNextestVersion}"
              "PKG_CONFIG_PATH=${pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" nativeLibs}"
              "CPATH=${pkgs.lib.makeSearchPathOutput "dev" "include" nativeLibs}"
              "LIBRARY_PATH=${pkgs.lib.makeLibraryPath nativeLibs}"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # The run is offline by construction; make a stray fetch fail loudly
              # rather than silently reach a network that a rebuild will not have.
              "CARGO_NET_OFFLINE=true"
            ];
            WorkingDir = "/src";
          };
        };

        # Convenience outputs, so version bumps can be inspected without
        # building the whole image.
        toolchain = rustToolchain;
        rust-script = rustScript;
        cargo-nextest = cargoNextest;
      };
    };
}
