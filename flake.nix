{
  description = "Local personal assistant and second brain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          rustfmt
          clippy
          rust-analyzer

          pkg-config
          openssl
          cmake
          gcc
          git
        ];

        shellHook = ''
          export ASSISTANT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
          export PATH="$ASSISTANT_ROOT/scripts:$PATH"

          echo "=========================================="
          echo " Personal Assistant Development Environment"
          echo "=========================================="
          echo "Rust:    $(rustc --version)"
          echo "Cargo:   $(cargo --version)"
          echo "OpenSSL: $(pkg-config --modversion openssl)"
          echo "CMake:   $(cmake --version | head -n 1)"
          echo
          echo "Launcher: assistant-tui"
        '';
      };
    };
}
