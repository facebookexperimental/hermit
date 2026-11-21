# Layer groups and composed images for RE-friendly hermetic testing.
#
# One definition, rendered per composed image. Only composed images
# are pinnable or selectable; bare layers are transfer accounting.
# This file is imported by flake.nix through one generic merge so a
# new family never edits flake.nix (whose bytes the monolith receipt
# binds). The monolith `.#image` must not reference anything here:
# non-dependence is proven by evaluating it with this file replaced
# by an aborting stub (recorded at each monolith re-receipt).
#
# Arguments are the shared bindings composed derivations read, each
# bound by hash in every composed receipt: pkgs (locked nixpkgs),
# requiredGuestPaths (from guest-paths.txt), archCoreutils.
{ pkgs, requiredGuestPaths, archCoreutils }:
let
  miniTools = with pkgs; [
    # base
    glibc glibc.bin bash coreutils
    # posix-utils
    findutils gnugrep gnused gawk diffutils gnutar gzip xz bzip2.bin zstd
    file jq bc cpio time gnupatch dos2unix zip unzip hostname ed
    # util-linux + procps
    util-linux procps
    # sys-fixtures (system-utils guests and their /bin fixtures)
    sysstat iproute2 kmod numactl lsof attr acl cscope gettext libxml2.bin
  ];
  # FHS guest-path links: link only providers this image carries.
  miniGuestPathLinks = ''
    for path in ${pkgs.lib.escapeShellArgs requiredGuestPaths}; do
      command="''${path##*/}"
      if [ "$command" = nodejs ]; then command=node; fi
      if [ -e "bin/$command" ]; then
        mkdir -p ".''${path%/*}"
        ln -s "/bin/$command" ".''${path}"
      fi
    done
  '';
  miniExtraCommands = ''
    mkdir -p tmp var/tmp usr/bin etc root lib64
    chmod 1777 tmp var/tmp
    chmod 0700 root
    printf 'root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\n' > etc/passwd
    printf 'root:x:0:\nnogroup:x:65534:\nnobody:x:65534:\n' > etc/group
    ln -sfn "${pkgs.glibc}/lib/ld-linux-x86-64.so.2" lib64/ld-linux-x86-64.so.2
    mkdir -p bin
    ln -s "${archCoreutils}/bin/arch" bin/arch
    ln -sfnT "${pkgs.procps}/bin/uptime" bin/uptime
    ${miniGuestPathLinks}
  '';
  miniConfig = {
    Env = [
      "PATH=/bin:/usr/bin"
      "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
    ];
    WorkingDir = "/src";
  };
  miniImage = pkgs.dockerTools.buildLayeredImage {
    name = "hermit-hermetic-mini";
    tag = "nix";
    maxLayers = 2;
    created = "1970-01-01T00:00:01Z";
    contents = miniTools ++ [
      pkgs.dockerTools.binSh
      pkgs.dockerTools.usrBinEnv
      pkgs.dockerTools.caCertificates
    ];
    extraCommands = miniExtraCommands;
    config = miniConfig;
  };
  extension = { name, from, contents }: pkgs.dockerTools.buildLayeredImage {
    inherit name;
    tag = "nix";
    fromImage = from;
    maxLayers = 4;
    created = "1970-01-01T00:00:01Z";
    inherit contents;
    extraCommands = ''
      mkdir -p bin usr/bin
      ${miniGuestPathLinks}
    '';
    config = miniConfig;
  };
  miniRunnerImage = extension {
    name = "hermit-hermetic-mini-runner";
    from = miniImage;
    contents = with pkgs; [ binutils libunwind python3 gitMinimal ];
  };
  miniCryptoImage = extension {
    name = "hermit-hermetic-mini-crypto";
    from = miniImage;
    contents = with pkgs; [ openssl openssh ];
  };
  miniPythonImage = extension {
    name = "hermit-hermetic-mini-python";
    from = miniImage;
    contents = [ pkgs.python3 ];
  };
in
{
  mini = miniImage;
  mini-runner = miniRunnerImage;
  mini-crypto = miniCryptoImage;
  mini-python = miniPythonImage;
  mini-suite = pkgs.dockerTools.buildLayeredImage {
    name = "hermit-hermetic-mini-suite";
    tag = "nix";
    fromImage = miniRunnerImage;
    maxLayers = 6;
    created = "1970-01-01T00:00:01Z";
    contents = with pkgs; [ openssl openssh ];
    extraCommands = ''
      mkdir -p bin usr/bin
      ${miniGuestPathLinks}
    '';
    config = miniConfig;
  };
}
