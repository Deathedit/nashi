{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  packages = with pkgs; [
    rustc
    cargo
    clippy
    rustfmt
    rust-analyzer
    pkg-config

    # conversion tools
    mame-tools # chdman
    extract-xiso
    zarchive
  ];

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
