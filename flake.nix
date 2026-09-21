# This flake file is community maintained
{
  description = "Niri: A scrollable-tiling Wayland compositor.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  # The M2 development VM only (nix/m2-vm.nix): a crosvm guest with the host GPU.
  inputs.microvm = {
    url = "github:astro/microvm.nix";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      microvm,
    }:
    let
      revision = self.shortRev or self.dirtyShortRev or "unknown";
      niri-package =
        {
          lib,
          cairo,
          dbus,
          libGL,
          libdisplay-info_0_3,
          libinput,
          seatd,
          libxkbcommon,
          libgbm,
          pango,
          pipewire,
          pkg-config,
          rustPlatform,
          systemd,
          wayland,
          installShellFiles,
          withDbus ? true,
          withSystemd ? true,
          withScreencastSupport ? true,
          withDinit ? false,
        }:

        rustPlatform.buildRustPackage {
          pname = "niri";
          version = revision;

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./niri-config
              ./drv-bridge
              ./drv-supervisor
              ./drv-appd
              ./drv-forker
              ./drv-trampoline
              ./drv-os
              ./niri-ipc
              ./drv-policy
              ./drv-seat
            ./drv-auth
            ./drv-lock
            ./drv-menu
            ./drv-portal
            ./drv-ui
              ./niri-visual-tests
              ./resources
              ./src
              ./Cargo.toml
              ./Cargo.lock
            ];
          };

          postPatch = ''
            patchShebangs resources/niri-session
            substituteInPlace resources/niri.service \
              --replace-fail 'ExecStart=niri' "ExecStart=$out/bin/niri"
          '';

          cargoLock = {
            lockFile = ./Cargo.lock;
            # The smithay fork pin lives on a branch, not master, so builtins.fetchGit cannot
            # find it by rev; fetch it by hash instead.
            outputHashes = {
              "smithay-0.7.0" = "sha256-VZFM7pV/fcOYnP0437x3lB0lDGxZ5b3DLXuhfltccUo=";
              "smithay-drm-extras-0.1.0" = "sha256-VZFM7pV/fcOYnP0437x3lB0lDGxZ5b3DLXuhfltccUo=";
            };
          };

          strictDeps = true;

          # The supervisor, drv-appd, drv-forker, the drv CLI, the bridge (with its chooser
          # probe) and the set's windows ship with the compositor.
          cargoBuildFlags = [
            "-p"
            "niri"
            "-p"
            "drv-supervisor"
            "-p"
            "drv-appd"
            "-p"
            "drv-forker"
            "-p"
            "drv-trampoline"
            "-p"
            "drv-bridge"
            "-p"
            "drv-seat"
            "-p"
            "drv-auth"
            "-p"
            "drv-lock"
            "-p"
            "drv-menu"
            "-p"
            "drv-portal"
          ];

          nativeBuildInputs = [
            rustPlatform.bindgenHook
            pkg-config
            installShellFiles
          ];

          buildInputs =
            [
              cairo
              dbus
              libGL
              libdisplay-info_0_3
              libinput
              seatd
              libxkbcommon
              libgbm
              pango
              wayland
            ]
            ++ lib.optional (withDbus || withScreencastSupport || withSystemd) dbus
            ++ lib.optional withScreencastSupport pipewire
            # Also includes libudev
            ++ lib.optional withSystemd systemd;

          buildFeatures =
            lib.optional withDbus "dbus"
            ++ lib.optional withDinit "dinit"
            ++ lib.optional withScreencastSupport "xdp-gnome-screencast"
            ++ lib.optional withSystemd "systemd"
            # No default features below applies to every package: drv-portal's binary is
            # behind its `service` feature (the compositor and the bridge want only its
            # protocols).
            ++ [ "drv-portal/service" ];
          buildNoDefaultFeatures = true;

          # ever since this commit:
          # https://github.com/niri-wm/niri/commit/771ea1e81557ffe7af9cbdbec161601575b64d81
          # niri now runs an actual instance of the real compositor (with a mock backend) during tests
          # and thus creates a real socket file in the runtime dir.
          # this is fine for our build, we just need to make sure it has a directory to write to.
          preCheck = ''
            export XDG_RUNTIME_DIR="$(mktemp -d)"
          '';

          checkFlags = [
            # These tests require the ability to access a "valid EGL Display", but that won't work
            # inside the Nix sandbox
            "--skip=::egl"
          ];

          postInstall =
            ''
              installShellCompletion --cmd niri \
                --bash <($out/bin/niri completions bash) \
                --fish <($out/bin/niri completions fish) \
                --nushell <($out/bin/niri completions nushell) \
                --zsh <($out/bin/niri completions zsh)

              install -Dm644 resources/niri.desktop -t $out/share/wayland-sessions
            ''
            + lib.optionalString withSystemd ''
              install -Dm755 resources/niri-session $out/bin/niri-session
              install -Dm644 resources/niri{.service,-shutdown.target} -t $out/lib/systemd/user
            '';

          env = {
            # Force linking with libEGL and libwayland-client so they end up in RPATH and
            # can be discovered by `dlopen()`
            RUSTFLAGS = toString (
              map (arg: "-C link-arg=" + arg) [
                "-Wl,--push-state,--no-as-needed"
                "-lEGL"
                "-lwayland-client"
                "-Wl,--pop-state"
              ]
            );
            NIRI_BUILD_COMMIT = revision;
          };

          passthru = {
            providedSessions = [ "niri" ];
          };

          meta = {
            description = "Scrollable-tiling Wayland compositor";
            homepage = "https://github.com/niri-wm/niri";
            license = lib.licenses.gpl3Only;
            mainProgram = "niri";
            platforms = lib.platforms.linux;
          };
        };

      inherit (nixpkgs) lib;
      # Support all Linux systems that the nixpkgs flake exposes
      systems = lib.intersectLists lib.systems.flakeExposed lib.platforms.linux;

      forAllSystems = lib.genAttrs systems;
      nixpkgsFor = forAllSystems (system: nixpkgs.legacyPackages.${system});
    in
    {
      checks = forAllSystems (system: {
        # We use the debug build here to save a bit of time
        inherit (self.packages.${system}) niri-debug;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
          rustfmt' = pkgs.rustfmt.override { asNightly = true; };
          inherit (self.packages.${system}) niri;
        in
        {
          default = pkgs.mkShell {
            packages = builtins.attrValues {
              inherit (pkgs)
                rustc
                cargo
                clippy
                cargo-insta
                ;
              inherit rustfmt';
            };

            nativeBuildInputs = [
              pkgs.rustPlatform.bindgenHook
              pkgs.pkg-config
              pkgs.wrapGAppsHook4 # For `niri-visual-tests`
            ];

            buildInputs = niri.buildInputs ++ [
              pkgs.libadwaita # For `niri-visual-tests`
            ];

            env = {
              # WARN: Do not overwrite this variable in your shell!
              # It is required for `dlopen()` to work on some libraries; see the comment
              # in the package expression
              #
              # This should only be set with `RUSTFLAGS="$RUSTFLAGS -C your-flags"`
              RUSTFLAGS = niri.RUSTFLAGS;
            };
          };
        }
      );

      formatter = forAllSystems (system: nixpkgsFor.${system}.nixfmt-rfc-style);

      packages = forAllSystems (
        system:
        let
          niri = nixpkgsFor.${system}.callPackage niri-package { };
        in
        {
          inherit niri;

          # NOTE: This is for development purposes only
          #
          # It is primarily to help with quickly iterating on
          # changes made to the above expression - though it is
          # also not stripped in order to better debug niri itself
          niri-debug = niri.overrideAttrs (
            newAttrs: oldAttrs: {
              pname = oldAttrs.pname + "-debug";

              cargoBuildType = "debug";
              cargoCheckType = newAttrs.cargoBuildType;

              dontStrip = true;
            }
          );

          default = niri;
        }
        // lib.optionalAttrs (system == "x86_64-linux") {
          # A development VM with the whole stack: seatd, virtio-gpu, root spawner with the
          # identity daemon, and the compositor on the tty backend. Host /src/niri is shared in for
          # iterating on host-built binaries. `nix build .#dev-vm && ./result/bin/run-*-vm`.
          dev-vm =
            (lib.nixosSystem {
              inherit system;
              modules = [
                (import ./nix/dev-vm.nix {
                  niri = self.packages.${system}.niri-debug.overrideAttrs (_: {
                    # Tests need an EGL display; the VM only needs the binaries.
                    doCheck = false;
                  });
                })
              ];
            }).config.system.build.vm;
          # Kernel blank-on-resume check, no desktop: `nix build .#resume-vm`, nix/resume-test.sh.
          resume-vm =
            (lib.nixosSystem {
              inherit system;
              modules = [ ./nix/resume-vm.nix ];
            }).config.system.build.vm;
        }
        // lib.optionalAttrs (system == "aarch64-linux") {
          # The same guest as a crosvm machine on an Apple M2 with the real GPU as a virtio-gpu
          # native context, its screen on a headless host compositor served over noVNC:
          # `nix/m2-vm-run.sh` on the host, `nix/m2-vm-exec` to drive it. niri comes prebuilt
          # from nix/m2-prebuilt (nix/cross-drv.sh on devbox), so the guest never compiles it.
          m2-vm =
            (lib.nixosSystem {
              inherit system;
              modules = [
                microvm.nixosModules.microvm
                (import ./nix/m2-vm.nix { niri = nixpkgsFor.${system}.callPackage ./nix/m2-prebuilt.nix { }; })
              ];
            }).config.microvm.declaredRunner;
        }
      );

      overlays.default = final: _: {
        niri = final.callPackage niri-package { };
      };

      # `services.drv`: the multi-UID desktop from one app list (nix/module.nix).
      nixosModules.default =
        { pkgs, ... }:
        {
          imports = [
            (import ./nix/module.nix { niri = self.packages.${pkgs.stdenv.hostPlatform.system}.niri; })
          ];
        };
    };
}
