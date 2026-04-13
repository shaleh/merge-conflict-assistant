{
  description = "Merge conflict assistant LSP and home-manager module";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      eachSystem = f: nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed (system:
        let
          pkgs = import nixpkgs {
            localSystem = { inherit system; };
            overlays = [ overlays.default ];
          };
        in
        f pkgs);

      merge-conflict-assistant = { lib, rustPlatform }:
        rustPlatform.buildRustPackage {
          inherit ((lib.importTOML ./Cargo.toml).package) name version;
          src = lib.cleanSourceWith {
            filter = path: _type: !lib.hasSuffix ".nix" path && !lib.hasSuffix ".png" path;
            src = lib.cleanSource ./.;
          };
          cargoLock.lockFile = ./Cargo.lock;
        };

      overlays.default = final: prev: {
        merge-conflict-assistant = prev.callPackage merge-conflict-assistant { };
      };
    in
    {
      inherit overlays;

      packages = eachSystem (pkgs:
        {
          inherit (pkgs) merge-conflict-assistant;
          default = pkgs.merge-conflict-assistant;
        });

      homeManagerModules.helix = { lib, pkgs, config, ... }:
        let
          inherit (pkgs.stdenv.hostPlatform) system;
          inherit (config.programs) helix;
          inherit (helix.languages.language-server) merge-conflict-assistant;
        in
        {
          options.programs.helix.merge-conflict-assistant = {
            enable = lib.mkEnableOption "Merge conflict assistant LSP";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${system}.default;
              description = "Merge conflict assistant LSP";
            };
          };
          
          config = lib.mkIf config.programs.helix.enable {
            programs.helix.languages = {
              language-server.merge-conflict-assistant = {
                command = "${config.programs.helix.merge-conflict-assistant.package}/bin/merge-conflict-assistant";
              };
            };
          };
        };
    };
}
