{
  description = "A powerful Git commit message analysis and amendment toolkit";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
          zlib
          libgit2
        ] ++ lib.optionals stdenv.hostPlatform.isDarwin [
          libiconv
        ];

      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage rec {
          pname = "omni-dev";
          version = "0.9.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          inherit nativeBuildInputs buildInputs;

          # Skip tests during build as they may require git repository setup
          doCheck = false;

          meta = with pkgs.lib; {
            description = "A powerful Git commit message analysis and amendment toolkit";
            homepage = "https://github.com/rust-works/omni-dev";
            license = licenses.bsd3;
            maintainers = [ ];
            mainProgram = "omni-dev";
          };
        };

        packages.omni-dev = self.packages.${system}.default;

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ (with pkgs; [
            rustc
            cargo
            cargo-watch
            cargo-edit
            clippy
            rustfmt
          ]);
        };
      });
}