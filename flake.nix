{
  description = "tar-dedup development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, fenix, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      rust = fenix.packages.${system}.stable.toolchain;
    in {
      devShells.${system}.default = pkgs.mkShell {
        name = "tar-dedup-dev";

        packages = [
          rust
          pkgs.pkg-config
          pkgs.xz        # liblzma for xz2
          pkgs.bzip2     # bzip2 for bzip2 crate
          pkgs.zstd      # zstd C lib for zstd crate
          pkgs.zlib      # zlib (used by flate2 in some configs)
        ];

        # Ensures dynamically linked C libs are findable at runtime
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
          pkgs.xz
          pkgs.bzip2
          pkgs.zstd
          pkgs.zlib
        ];

        # pkg-config paths so build scripts can find the libs
        PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" [
          pkgs.xz.dev
          pkgs.bzip2.dev
          pkgs.zstd.dev
          pkgs.zlib.dev
        ];

        shellHook = ''
          echo "🦀 tar-dedup dev shell — $(rustc --version)"
        '';
      };
    };
}
