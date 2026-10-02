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
        # build/ is gitignored, so it is not in self.outPath; BUSYBEE_REPO (set
        # by scripts/buildanddeploy.sh, needs --impure) points at the worktree.
        repoRoot = builtins.getEnv "BUSYBEE_REPO";
        binaryStorePath = builtins.path {
          path = "${repoRoot}/build/release/busybee";
          name = "busybee-bin";
        };
        bzbStorePath = builtins.path {
          path = "${repoRoot}/build/release/bzb";
          name = "bzb-bin";
        };
        bzbdStorePath = builtins.path {
          path = "${repoRoot}/build/release/bzbd";
          name = "bzbd-bin";
        };
      in
      {
        # Binary-only, non-hermetic: packages binaries already built under
        # `nix develop` into build/release/ (see scripts/buildanddeploy.sh), so
        # cargo build scripts never have to run inside the nix sandbox.
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
            # bzbd must sit next to bzb: that is where the client looks for it
            # before falling back to PATH.
            cp "${bzbdStorePath}" $out/bin/bzbd
            chmod +x $out/bin/bzbd
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
            # PreToolUse hook (sortie/machine-safety-hook.sh) parses JSON with jq.
            # A missing jq makes the hook exit 127, which Claude Code does not
            # treat as a deny.
            pkgs.jq
            # Agent review packets and harness tests; pyte replays VM lab
            # terminal recordings into screen cells.
            (pkgs.python3.withPackages (ps: [ ps.pyte ps.pillow ps.fonttools ]))
            # Renders those recordings to images, with the pinned font below.
            pkgs.asciinema-agg
          ];
          BUSYBEE_TERMINAL_FONTS = "${pkgs.dejavu_fonts}/share/fonts/truetype";
        };

        # Optional orchestrator tools; no host profile or service activation.
        devShells.agent = pkgs.mkShell {
          inputsFrom = [ self.devShells.${system}.default ];
          packages = [
            (import ./sortie/runtime.nix { inherit pkgs; })
            pkgs.gh
            pkgs.actionlint
          ];
        };
      });
}
