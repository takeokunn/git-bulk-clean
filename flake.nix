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
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.makeWrapper pkgs.scdoc ];

            postInstall = ''
              wrapProgram $out/bin/git-bulk-clean \
                --prefix PATH : ${pkgs.lib.makeBinPath [
                  pkgs.git
                  pkgs.ghq
                  pkgs.coreutils
                ]}

              install -Dm644 <($out/bin/git-bulk-clean --generate-completions bash) \
                $out/share/bash-completion/completions/git-bulk-clean
              install -Dm644 <($out/bin/git-bulk-clean --generate-completions zsh) \
                $out/share/zsh/site-functions/_git-bulk-clean
              install -Dm644 <($out/bin/git-bulk-clean --generate-completions fish) \
                $out/share/fish/vendor_completions.d/git-bulk-clean.fish

              mkdir -p $out/share/man/man1
              scdoc < man/git-bulk-clean.1.scd > $out/share/man/man1/git-bulk-clean.1
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
