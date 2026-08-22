{
  description = "busybee — queued runner for resource-heavy tasks with a live CPU+queue monitor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Resolve the repo root from the flake's own source tree.
        # self.outPath is the path to the flake source in the nix store, but
        # build/ is gitignored so it won't be there. Instead we use the
        # BUSYBEE_REPO env var (set by scripts/buildanddeploy.sh) so that
        # builtins.path can find the binary in the working tree. Requires
        # `--impure` or BUSYBEE_REPO to be set.
        repoRoot = builtins.getEnv "BUSYBEE_REPO";
        binaryStorePath = builtins.path {
          path = "${repoRoot}/build/release/busybee";
          name = "busybee-bin";
        };
        bzbStorePath = builtins.path {
          path = "${repoRoot}/build/release/bzb";
          name = "bzb-bin";
        };
      in
      {
        # Binary-only derivation: copies the pre-built release binary from
        # `build/release/busybee` into the nix store. Non-hermetic by design
        # — see scripts/buildanddeploy.sh for the end-to-end flow.
        #
        # Why: cargo's build-script-build executables are blocked by the
        # user's local binary-execution policy, so we can't run cargo
        # inside the nix sandbox. We build under `nix develop` (where
        # compiled artifacts land in `./build/release/`) and then have nix
        # package the resulting binary.
        packages.default = pkgs.stdenv.mkDerivation {
          pname = "busybee";
          version = "0.1.0";
          dontUnpack = true;
          dontConfigure = true;
          dontBuild = true;
          installPhase = ''
            runHook preInstall
            mkdir -p $out/bin
            cp "${binaryStorePath}" $out/bin/busybee
            chmod +x $out/bin/busybee
            cp "${bzbStorePath}" $out/bin/bzb
            chmod +x $out/bin/bzb
            runHook postInstall
          '';
          meta = {
            description = "Queued task runner with live CPU+queue TUI";
            mainProgram = "busybee";
            platforms = pkgs.lib.platforms.unix;
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.rustfmt
            pkgs.clippy
            pkgs.pueue
            # Jobserver integration tests (crates/bzb-core/tests/jobserver.rs)
            # need make >= 4.4 and ninja >= 1.13.
            pkgs.gnumake
            pkgs.ninja
            pkgs.git
            pkgs.pkg-config
          ];
        };
      });
}
