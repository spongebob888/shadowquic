{
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        muslCc = pkgs.pkgsCross.musl64.stdenv.cc;
        muslTargetPrefix = muslCc.targetPrefix;

        libraries = with pkgs;[
          glib
          openssl_3.dev
          sqlite
          libclang
          stdenv.cc.cc.lib
        ];
        pythonEnv = pkgs.python3.withPackages (ps: with ps; [
	          pycurl
            pysocks
            dnspython
          ]);

        mkPackage = targetPkgs:
          let craneLib = crane.mkLib targetPkgs;
          in craneLib.buildPackage {
            src = craneLib.cleanCargoSource ./.;
            pname = "shadowquic";
            doCheck = false;
            cargoExtraArgs = "--no-default-features --features shadowquic-quinn,sunnyquic-noq,ring,statistics,tproxy,mixed,plugin-system,router-db,dns-server,router-dhcp-lease,restful-api,tun,jemalloc";
            nativeBuildInputs = [ targetPkgs.buildPackages.pkg-config ];
            buildInputs = [ targetPkgs.luajit ];
          };
        packages = with pkgs; [
          curlHTTP3
          pythonEnv
          wget
          sqlite
          pkg-config
          openssl_3
          glib
          cmake
          protobuf
          protoc-gen-rust
		      clang
          samply
          iperf3
          sing-box
          killall
          act
          rustup
          uv
          muslCc
          luajit
        ];
      in
      {
        devShell = pkgs.mkShell {
          buildInputs = packages;

          shellHook =
            ''
              # Cargo's cc crate builds C dependencies with -O0 for dev builds.
              # Nix's cc-wrapper adds fortify hardening by default, and glibc's
              # fortify headers then emit a #warning that jemalloc's configure
              # promotes to an error with -Werror.  Drop fortify flags while
              # keeping the remaining hardening flags.
              export NIX_HARDENING_ENABLE="$(
                if [ -n "$NIX_HARDENING_ENABLE" ]; then
                  for flag in $NIX_HARDENING_ENABLE; do
                    case "$flag" in
                      fortify|fortify3) ;;
                      *) printf '%s ' "$flag" ;;
                    esac
                  done
                fi
              )"
              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath libraries}:$LD_LIBRARY_PATH
              # Nix's rustup linker wrappers embed absolute toolchain paths.
              # Cross 0.2.5 mounts the toolchain at /rust, so also keep its host path.
              export CROSS_CONTAINER_OPTS="''${CROSS_CONTAINER_OPTS:+$CROSS_CONTAINER_OPTS }--volume=''${RUSTUP_HOME:-$HOME/.rustup}:''${RUSTUP_HOME:-$HOME/.rustup}:ro"
              export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=${muslCc}/bin/${muslTargetPrefix}gcc
              export CC_x86_64_unknown_linux_musl=${muslCc}/bin/${muslTargetPrefix}gcc
              export CXX_x86_64_unknown_linux_musl=${muslCc}/bin/${muslTargetPrefix}g++
              export LUAU_CXXFLAGS=-U_FORTIFY_SOURCE
              export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:$XDG_DATA_DIRS
            '';
        };
        packages = {
          default = mkPackage pkgs;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          musl = mkPackage pkgs.pkgsStatic;
        };
      });
}
