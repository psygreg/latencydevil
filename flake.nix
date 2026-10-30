{
  description = "GPU-aware launcher wrapper to set low latency modes for gaming";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      # latencydevil is Linux-specific (relies on Linux sysfs at /sys/bus/pci/devices)
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          latencydevil = pkgs.rustPlatform.buildRustPackage {
            pname = "latencydevil";
            version = "0.2.0";

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            meta = with pkgs.lib; {
              description = "GPU-aware launcher wrapper: injects Wayland/low-latency variables into a wrapped command";
              homepage = "https://github.com/psygreg/latencydevil";
              license = licenses.asl20;
              mainProgram = "latencydevil";
              platforms = platforms.linux;
            };
          };

          default = self.packages.${system}.latencydevil;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/latencydevil";
        };
      });

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
            ];
          };
        }
      );
    };
}
