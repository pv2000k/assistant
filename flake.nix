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
          echo "=========================================="
          echo " Personal Assistant Development Environment"
          echo "=========================================="
          echo "Rust:    $(rustc --version)"
          echo "Cargo:   $(cargo --version)"
          echo "OpenSSL: $(pkg-config --modversion openssl)"
          echo "CMake:   $(cmake --version | head -n 1)"
          echo "Ladybug: $(lbug --version)"
          echo
        '';
      };
    };
}
