{
  description = "Cache non-interactive command output and exit status";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      packageFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = manifest.package.name;
          version = manifest.package.version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.lock
              ./Cargo.toml
              ./LICENSE
              ./README.md
              ./src
              ./tests
            ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            inherit (manifest.package) description;
            homepage = manifest.package.repository;
            license = pkgs.lib.licenses.mit;
            mainProgram = "cacheexec";
            platforms = pkgs.lib.platforms.unix;
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system;
        cacheexec = packageFor system;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/cacheexec";
          meta.description = manifest.package.description;
        };
      });

      checks = forAllSystems (system: {
        cacheexec = self.packages.${system}.default;
      });
    };
}
