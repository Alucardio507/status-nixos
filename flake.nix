{
  description = "CLI NixOS channel status display";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # Reuse nixpkgs' own instance instead of re-importing it per output.
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # release.json is written by the release workflow (.github/workflows/release.yml).
      # It holds the version plus the URL and hash of each prebuilt binary.
      release = builtins.fromJSON (builtins.readFile ./release.json);
      cargoVersion = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;

      metaFor = pkgs: {
        description = "Show NixOS channel status in the terminal";
        homepage = "https://status.nixos.org/";
        license = pkgs.lib.licenses.mit;
        mainProgram = "status-nixos";
        platforms = systems;
      };

      # Builds from source with cargo. Used for development, CI, and as a
      # fallback before the first release exists.
      sourcePackageFor =
        system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "status-nixos";
          version = cargoVersion;
          # Only what cargo needs. Editing README, workflows, release.json,
          # scripts, ... does not change this source, so nothing rebuilds.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = true;
          meta = metaFor pkgs;
        };

      # Downloads the prebuilt binary from GitHub Releases. No compiling.
      binaryPackageFor =
        system: asset:
        let
          pkgs = pkgsFor system;
        in
        pkgs.stdenv.mkDerivation {
          pname = "status-nixos";
          version = release.version;
          src = pkgs.fetchurl { inherit (asset) url hash; };
          dontUnpack = true;
          dontConfigure = true;
          dontBuild = true;
          # Linux binaries are static (musl); do not strip/patch anything,
          # that would also break macOS code signatures.
          dontFixup = true;
          installPhase = ''
            mkdir -p $out/bin
            tar -xzf $src -C $out/bin status-nixos
            chmod +x $out/bin/status-nixos
          '';
          meta = (metaFor pkgs) // {
            sourceProvenance = [ pkgs.lib.sourceTypes.binaryNativeCode ];
          };
        };

      defaultPackageFor =
        system:
        if release.assets ? ${system} then
          binaryPackageFor system release.assets.${system}
        else
          sourcePackageFor system;
    in
    {
      packages = forAllSystems (system: rec {
        default = defaultPackageFor system;
        status-nixos = default;
        # Always compiles from source, ignoring release.json.
        source = sourcePackageFor system;
      });

      # `nix flake check`: builds from source and runs `cargo test` in the sandbox.
      checks = forAllSystems (system: {
        source = self.packages.${system}.source;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/status-nixos";
        };
        status-nixos = self.apps.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              cargo-bloat # `just bloat`: what takes space in the binary
              just # task runner, see justfile
              git
              nixfmt
            ];
            # Lets rust-analyzer find the std sources.
            env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
