{
  description = "Minimal native PipeWire audio control panel for Wayland";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems
          (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAllSystems (pkgs:
        let
          sink = pkgs.rustPlatform.buildRustPackage {
            pname = "sink";
            version = "0.1.0";

            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = with pkgs; [
              pkg-config
              wrapGAppsHook4
              clang
              glib
            ];
            buildInputs = with pkgs; [
              gtk4
              pipewire
            ];

            # libspa-sys uses bindgen for the PipeWire headers.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            postInstall = ''
              install -Dm644 ${./dev.saku.Sink.desktop} \
                "$out/share/applications/dev.saku.Sink.desktop"
              install -Dm644 ${./icons/scalable/apps/sink.svg} \
                "$out/share/icons/hicolor/scalable/apps/sink.svg"
            '';

            meta = with pkgs.lib; {
              description = "Minimal PipeWire audio control panel for Wayland";
              homepage = "https://github.com/sakuyma/sink";
              license = licenses.mit;
              mainProgram = "sink";
              platforms = platforms.linux;
            };
          };
        in {
          default = sink;
          inherit sink;
        });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            pkg-config
            wrapGAppsHook4
            clang
            glib
          ];
          buildInputs = with pkgs; [
            gtk4
            pipewire
            wireplumber
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };
      });
    };
}
