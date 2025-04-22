{
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        libraries = with pkgs;[
          glib
          openssl_3.dev
          sqlite
          libclang
        ];
        pythonEnv = pkgs.python3.withPackages (ps: with ps; [
	          pycurl
            pysocks
            dnspython
          ]);


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
        ];
      in
      {
        devShell = pkgs.mkShell {
          buildInputs = packages;

          shellHook =
            ''
              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath libraries}:$LD_LIBRARY_PATH
              export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:$XDG_DATA_DIRS
            '';
        };
      });
}
