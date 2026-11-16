{
  description = "screentime: screen time limits, downtime schedules and time requests for Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };

          # Build in a persistent target directory (/var/tmp/cargo-nix, exposed to the sandbox by
          # the system flake) so rebuilds only recompile what changed, like nixos-update-checker
          # and Kog. Falls back to a throwaway directory when the sandbox doesn't expose it.
          # kache stays out of these builds: CARGO_HOME is the build's own.
          persistentCargo = name: cargoArgs: ''
            runHook preBuild
            workDir=$TMPDIR/cargo-nix
            if [ -w /var/tmp/cargo-nix ]; then
              workDir=/var/tmp/cargo-nix/${name}
            fi
            # The store vendor copy only changes with Cargo.lock; the per-build copy gets new
            # mtimes, which would rebuild every dependency.
            sed -i "s|^directory = .*|directory = \"${cargoDeps}\"|" "$NIX_BUILD_TOP/.cargo/config.toml"
            (
              # Everything stays writable by everyone: each build may run as a different user.
              # Kept to this subshell so the outputs get normal permissions.
              umask 000
              mkdir -p "$workDir/src" "$workDir/target"
              touch "$workDir"
              # Only rewrite files whose content changed; cargo's freshness check relies on mtimes.
              rsync -rc --no-times --no-perms --chmod=a+rwX --delete ./ "$workDir/src/"
              # The linker wrapper rejects output paths outside the build dir.
              export NIX_ENFORCE_PURITY=0
              cd "$workDir/src"
              CARGO_HOME="$NIX_BUILD_TOP/.cargo" CARGO_TARGET_DIR="$workDir/target" \
                cargo build ${cargoArgs} --offline --locked -j "$NIX_BUILD_CORES"
              find "$workDir/target" -mindepth 1 -user "$(id -u)" ! -perm -o+w -exec chmod a+rwX {} + 2>/dev/null || true
            )
            ln -sfn "$workDir/target" target
            runHook postBuild
          '';
        in
        rec {
          # The Leptos UI, compiled to WebAssembly.
          web = pkgs.rustPlatform.buildRustPackage {
            pname = "screentime-web";
            version = "1.0.0";
            src = pkgs.lib.cleanSource ./.;
            inherit cargoDeps;
            nativeBuildInputs = [ pkgs.wasm-bindgen-cli pkgs.lld pkgs.rsync ];
            buildPhase = persistentCargo "screentime-web"
              "--profile wasm-release --target wasm32-unknown-unknown -p screentime-web";
            installPhase = ''
              runHook preInstall
              mkdir -p $out
              wasm-bindgen --target web --no-typescript --out-dir $out \
                target/wasm32-unknown-unknown/wasm-release/screentime_web.wasm
              cp crates/web/static/* $out/
              runHook postInstall
            '';
            doCheck = false;
          };

          default = pkgs.rustPlatform.buildRustPackage {
            pname = "screentime";
            version = "1.0.0";
            src = pkgs.lib.cleanSource ./.;
            inherit cargoDeps;
            nativeBuildInputs = [ pkgs.mold pkgs.rsync ];
            buildInputs = [ pkgs.pam ];
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
            buildPhase = persistentCargo "screentime"
              "--release --workspace --exclude screentime-web --exclude screentime-windows --exclude screentime-credprov";
            installPhase = ''
              runHook preInstall
              install -Dm755 -t $out/bin target/release/{screentime,screentime-client,screentime-greeter,screentime-server}
              install -Dm755 target/release/libpam_screentime.so $out/lib/security/pam_screentime.so
              mkdir -p $out/share/screentime
              ln -s ${web} $out/share/screentime/web
              runHook postInstall
            '';
            doCheck = false;
          };

          # The server as a container image, for running under Docker:
          #   nix build .#docker && docker load < result
          # Data lives in /data; TLS comes from SCREENTIME_TLS_CERT/KEY (reloaded when renewed).
          docker = pkgs.dockerTools.buildLayeredImage {
            name = "screentime";
            tag = "latest";
            contents = [ pkgs.cacert pkgs.tzdata ];
            # Timezone data, so TZ=America/Los_Angeles works: without it the server's "today" is
            # UTC's, which rolls over at 4 PM in California.
            extraCommands = ''
              mkdir -p data tmp usr/share etc && chmod 1777 tmp
              ln -s ${pkgs.tzdata}/share/zoneinfo usr/share/zoneinfo
            '';
            config = {
              Cmd = [ "${default}/bin/screentime-server" "serve" ];
              Env = [
                "SCREENTIME_LISTEN=0.0.0.0:8443"
                "SCREENTIME_DB=sqlite:///data/screentime.db"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
                "TZDIR=${pkgs.tzdata}/share/zoneinfo"
              ];
              ExposedPorts."8443/tcp" = { };
              Volumes."/data" = { };
            };
          };
        }
      );

      nixosModules = {
        server = import ./nix/server.nix self;
        client = import ./nix/client.nix self;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer sqlx-cli wasm-bindgen-cli lld pam ];
        };
      });
    };
}
