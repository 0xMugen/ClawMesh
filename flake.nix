{
  description = "ClawMesh — type-safe agent protocol + private mesh networking for OpenClaw";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            git
            gh
            just
            jq
            yq-go
            nodejs_22
            pnpm
            python3
            uv
            rustc
            cargo
            rustfmt
            clippy
          ];

          shellHook = ''
            echo "🕸️ ClawMesh dev shell"
            echo "Tools: git, gh, just, jq, yq, node/pnpm, python/uv, rust/cargo"
          '';
        };
      });
}
