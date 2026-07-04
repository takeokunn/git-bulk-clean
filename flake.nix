{
  description = "git-bulk-clean: parallel Git repository maintenance daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      homeManagerModules.default = import ./hm-module.nix { inherit self; };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          git-bulk-clean = pkgs.rustPlatform.buildRustPackage {
            pname = "git-bulk-clean";
            version = "0.2.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.makeWrapper pkgs.scdoc pkgs.installShellFiles ];

            # `cargo test` runs in the check phase; the suite shells out to `git`,
            # and clippy/rustfmt gate quality so `nix flake check` is the single
            # entry point for the whole test/lint pipeline.
            nativeCheckInputs = [ pkgs.git pkgs.clippy pkgs.rustfmt ];
            preCheck = ''
              export HOME=$TMPDIR
              cargo fmt --check
              cargo clippy --all-targets -- -D warnings
            '';

            postInstall = ''
              wrapProgram $out/bin/git-bulk-clean \
                --prefix PATH : ${pkgs.lib.makeBinPath [
                  pkgs.git
                  pkgs.ghq
                  pkgs.coreutils
                ]}

              # Generate completions to real files first, then install under the
              # canonical names each shell's autoloader expects. Piping straight
              # into `install <(...)` races on /dev/fd in the build sandbox.
              $out/bin/git-bulk-clean --generate-completions bash > completion.bash
              $out/bin/git-bulk-clean --generate-completions zsh  > completion.zsh
              $out/bin/git-bulk-clean --generate-completions fish > completion.fish
              install -Dm644 completion.bash $out/share/bash-completion/completions/git-bulk-clean
              install -Dm644 completion.zsh  $out/share/zsh/site-functions/_git-bulk-clean
              install -Dm644 completion.fish $out/share/fish/vendor_completions.d/git-bulk-clean.fish

              scdoc < man/git-bulk-clean.1.scd > git-bulk-clean.1
              installManPage git-bulk-clean.1
            '';

            meta = {
              description = "Parallel Git repository maintenance CLI/daemon";
              homepage = "https://github.com/takeokunn/git-bulk-clean";
              license = pkgs.lib.licenses.mit;
              maintainers = [ pkgs.lib.maintainers.takeokunn ];
              mainProgram = "git-bulk-clean";
            };
          };
        in
        { default = git-bulk-clean; });

      # `nix flake check` builds this derivation, whose check phase runs
      # rustfmt --check, clippy -D warnings, and the full cargo test suite.
      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      apps = forAllSystems (system:
        {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/git-bulk-clean";
          };
        });

      devShells = forAllSystems (system:
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
              git
              ghq
              nixd
            ];
            shellHook = ''
              cat <<'USAGE_EOF'

=== git-bulk-clean Development Shell ===

Build & run:
  cargo build           # Debug build
  cargo build --release # Release build
  cargo run -- --help   # Run with args

Test & lint:
  cargo test            # Run all tests
  cargo clippy          # Lint
  cargo fmt             # Format

Nix build:
  nix build             # Build via Nix (uses Cargo.lock)
  nix flake check       # Run checks in sandbox

USAGE_EOF
            '';
          };
        });

      inherit homeManagerModules;
    };
}
