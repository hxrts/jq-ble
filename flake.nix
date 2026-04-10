{
  description = "Jacquard adapters for blew BLE";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    toolkit = {
      url = "github:hxrts/rust-toolkit";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      toolkit,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [
          (import rust-overlay)
        ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        };

        toolkitSupport = toolkit.lib.${system}.consumerShellSupport;

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          just
          perl
          ripgrep
        ] ++ toolkitSupport.packages;

        buildInputs =
          with pkgs;
          [
            openssl
          ]
          ++ toolkitSupport.buildInputs;

      in
      {
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          shellHook = ''
            [[ -r "$HOME/.local/state/secrets/cargo-registry-token" ]] && export CARGO_REGISTRY_TOKEN="$(cat "$HOME/.local/state/secrets/cargo-registry-token")"
            ${toolkitSupport.shellHook}

            echo "jq-ble development environment"
            echo "Rust: $(rustc --version)"
          '';
        };
      }
    );
}
