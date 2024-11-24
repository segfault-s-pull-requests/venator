# https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/su/surrealist/package.nix
# https://github.com/NixOS/nixpkgs/blob/10c475aeb9d30451786d9d4319b4861dce7febca/doc/hooks/tauri.section.md#L4

{
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = inputs:
    with inputs;
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = nixpkgs.lib;
      in rec {
        # packages.default = pkgs.rustPlatform.buildRustPackage {
        #   pname = "venator-app";
        #   version = "0.1.0";

        #   inherit buildInputs nativeBuildInputs;

        #   preBuild = ''
        #     cd venator-app;
        #     npm run build
        #   '';

        #   src = pkgs.lib.cleanSource ./.;

        #   cargoLock.lockFile = ./Cargo.lock;
        # };

        packages.default = pkgs.rustPlatform.buildRustPackage rec {
          # . . .
          pname = "venator";
          version = "0.2.1";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          # Assuming our app's frontend uses `npm` as a package manager
          npmDeps = pkgs.fetchNpmDeps {
            name = "${pname}-npm-deps-${version}";
            src = "${src}/venator-app";
            hash = "sha256-e+DE0eOxjTQimr75LZKVD19qHhdxtLAKHhKAucCTLpk=";
          };
          npmRoot = "venator-app";

          nativeBuildInputs = with pkgs; [
            # Pull in our main hook
            cargo-tauri.hook

            # Setup npm
            nodejs
            npmHooks.npmConfigHook

            # Make sure we can find our libraries
            pkg-config
            wrapGAppsHook4
          ];

          buildInputs = with pkgs;
            [
              webkitgtk_4_1
              gtk3
              cairo
              gdk-pixbuf
              glib
              dbus
              openssl
              libsoup_3
              librsvg
            ];

          # And make sure we build there too
          buildAndTestSubdir = "venator-app/src-tauri";

          meta = {
            description = "Venator: a log and trace viewer for Rust tracing";
            homepage = "https://github.com/kmdreko/venator";
            license = lib.licenses.mit;
            # maintainers = [ ];
          };
        };

        devShells.default = pkgs.mkShell {
          # Rust Analyzer needs to be able to find the path to default crate
          # sources, and it can read this environment variable to do so. The
          # `rust-src` component is required in order for this to work.
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          inputsFrom = [ packages.default ];

          # Development tools
          nativeBuildInputs = with pkgs; [
            cargo

            nixd
            rust-analyzer
            nodejs
          ];
        };

        checks = {
          packagesDefault = self.packages.${system}.default;
          devShellsDefault = self.devShells.${system}.default;
        };
      });
}

