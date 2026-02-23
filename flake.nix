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

        gatewayPkg = pkgs.rustPlatform.buildRustPackage {
          pname = "clawmesh-gateway";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "gateway" ];
          doCheck = false;
        };
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

        packages = {
          default = gatewayPkg;
          gateway = gatewayPkg;

          dockerImage = pkgs.dockerTools.buildImage {
            name = "clawmesh-gateway";
            tag = "latest";

            copyToRoot = pkgs.buildEnv {
              name = "image-root";
              paths = [
                gatewayPkg
                pkgs.busybox
                pkgs.cacert
              ];
              pathsToLink = [ "/bin" "/etc" ];
            };

            config = {
              Cmd = [ "${gatewayPkg}/bin/gateway" ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              Env = [
                "RUST_LOG=info"
                "GATEWAY_ADDR=0.0.0.0:8080"
                "MESH_ID=mesh:default"
                "NODE_ID=node:gateway-0"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              WorkingDir = "/";
            };
          };
        };

        apps = {
          default = {
            type = "app";
            program = "${gatewayPkg}/bin/gateway";
          };

          loadDocker = {
            type = "app";
            program = toString (pkgs.writeShellScript "load-docker" ''
              echo "Loading ClawMesh Docker image..."
              ${self.packages.${system}.dockerImage} | ${pkgs.docker}/bin/docker load
              echo "Loaded. Run with: docker run -p 8080:8080 clawmesh-gateway:latest"
            '');
          };
        };
      });
}
