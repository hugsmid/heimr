{
  description = "heimr: a self-hosted, local-first home brain in Rust";

  inputs = {
    nixpkgs.url = "git+https://github.com/NixOS/nixpkgs?ref=nixos-26.05&shallow=1";
    rust-overlay = {
      url = "git+https://github.com/oxalica/rust-overlay?shallow=1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "git+https://github.com/ipetkov/crane?shallow=1";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
      ...
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f (pkgsFor system));

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

      rustFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      craneFor =
        pkgs:
        let
          craneLib = (crane.mkLib pkgs).overrideToolchain rustFor;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./deny.toml
              ./clippy.toml
              # crates/heimr/tests/guide.rs checks the guide against the code.
              ./docs/writing-an-adapter.md
              # heimr-testkit compiles and tests the adapter template.
              ./templates
              # Hand-built /proc and /sys trees (files without extensions).
              ./crates/heimr-system/tests/fixtures
              (craneLib.fileset.commonCargoSources ./crates)
              (lib.fileset.fileFilter (
                f:
                lib.any f.hasExt [
                  "snap"
                  "json"
                  "md"
                  "toml"
                  "txt" # proptest-regressions/*.txt: persisted failure seeds
                ]
              ) ./crates)
            ];
          };
          commonArgs = {
            inherit src;
            pname = "heimr-workspace";
            version = "0.0.0";
            strictDeps = true;
            # jiff needs a tz database in tests.
            TZDIR = "${pkgs.tzdata}/share/zoneinfo";
            # rustls-platform-verifier needs a CA bundle even for local tests.
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake # aws-lc-sys (rustls default provider)
            ];
            buildInputs = lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          inherit
            craneLib
            commonArgs
            cargoArtifacts
            src
            ;
          # The app with the adapters that ship with it.
          heimr = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "heimr";
              cargoExtraArgs = "-p heimr";
              doCheck = false;
              meta.mainProgram = "heimr";
            }
          );
        };
    in
    {
      # Adds `heimr` (built with the pinned toolchain) to a package set.
      overlays.default = lib.composeManyExtensions [
        rust-overlay.overlays.default
        (final: _prev: { inherit (craneFor final) heimr; })
      ];

      packages = forAllSystems (pkgs: {
        default = (craneFor pkgs).heimr;
        inherit (craneFor pkgs) heimr;
      });

      devShells = forAllSystems (
        pkgs:
        let
          c = craneFor pkgs;
        in
        {
          default = c.craneLib.devShell {
            checks = self.checks.${pkgs.stdenv.hostPlatform.system};
            packages = with pkgs; [
              rust-analyzer
              cargo-deny
              cargo-insta
              cargo-nextest
              gitleaks
              nixfmt
              jq
            ];
          };
        }
      );

      checks = forAllSystems (
        pkgs:
        let
          c = craneFor pkgs;
          inherit (c) craneLib commonArgs cargoArtifacts;
        in
        {
          inherit (c) heimr;
          fmt = craneLib.cargoFmt { inherit (c) src; };
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            }
          );
          test = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoTestExtraArgs = "--workspace";
            }
          );
          # Advisories need network access, so they run in CI via
          # cargo-deny-action. Licences, bans and sources are checked offline.
          deny = craneLib.cargoDeny (
            commonArgs
            // {
              cargoDenyChecks = "bans licenses sources";
            }
          );
          nixfmt =
            pkgs.runCommand "nixfmt-check"
              {
                nativeBuildInputs = [ pkgs.nixfmt ];
                src = lib.fileset.toSource {
                  root = ./.;
                  fileset = lib.fileset.fileFilter (f: f.hasExt "nix") ./.;
                };
              }
              ''
                cd $src
                find . -name '*.nix' -print0 | xargs -0 nixfmt --check
                touch $out
              '';
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
