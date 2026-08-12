{
  description = "vim-zellij-navigator dev shell (zellij plugin, Rust -> wasm32-wasip1)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs;
            [ rustup ]
            ++ lib.optionals stdenv.isDarwin [ libiconv ];

          # The system-wide SDKROOT/DEVELOPER_DIR (set in this machine's
          # nix-darwin config) point at an apple-sdk package that's missing
          # usr/lib/*.tbd stubs (libiconv among them), which breaks linking
          # host build scripts. Unset them here so clang falls back to
          # whatever `xcode-select` points at (the actual Xcode.app install,
          # which has a complete SDK) instead of inheriting the broken ones.
          shellHook = ''
            unset SDKROOT
            unset DEVELOPER_DIR
            rustup target add wasm32-wasip1 >/dev/null 2>&1 || true
          '';
        };
      });
}
