{
  description = "git-bulk-clean: parallel Git repository maintenance daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      # homeManagerModules is system-agnostic, so it lives outside the
      # per-system loop and receives `self` so the module can resolve the
      # package for the caller's system at evaluation time.
      homeManagerModules.default = import ./hm-module.nix { inherit self; };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

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

            # completions (generated from the binary itself)
            install -Dm644 <($out/bin/git-bulk-clean --generate-completions bash) \
              $out/share/bash-completion/completions/git-bulk-clean
            install -Dm644 <($out/bin/git-bulk-clean --generate-completions zsh) \
              $out/share/zsh/site-functions/_git-bulk-clean
            install -Dm644 <($out/bin/git-bulk-clean --generate-completions fish) \
              $out/share/fish/vendor_completions.d/git-bulk-clean.fish

            # man page
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
      {
        packages.default = git-bulk-clean;

        apps.default = flake-utils.lib.mkApp {
          drv = git-bulk-clean;
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            git
            ghq
          ];
        };
      }
    ) // { inherit homeManagerModules; };
}
