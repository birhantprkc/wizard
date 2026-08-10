{
  description = "wizard — One line. Your sovereign agent. Self-extending. Bring any model.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      mkWizard =
        pkgs:
        let
          inherit (pkgs) lib;
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          # Runtime tools wizard shells out to for the default loadout:
          # nodejs provides npx (Playwright MCP), llama-cpp provides
          # llama-server, and ollama for local models.
          runtimeBins = [
            pkgs.nodejs
            pkgs.llama-cpp
            pkgs.ollama
          ];
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "wizard";
          version = cargoToml.package.version;

          src = ./.;

          # Every dependency is a crates.io dependency (Cargo.toml says why), so
          # buildRustPackage vendors the whole tree from the lock's own hashes.
          # No `outputHashes` here: that attribute exists only for git
          # dependencies, whose fixed-output hashes have to be supplied by hand
          # and then drift from the lock every time the pinned revision moves.
          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Put the optional runtime helpers on PATH so the default loadout
          # (Playwright MCP via npx, local models via llama-server/ollama)
          # works out of the box.
          postInstall = ''
            wrapProgram $out/bin/wizard \
              --prefix PATH : ${lib.makeBinPath runtimeBins}
          '';

          # The integration tests (tests/cli.rs, tests/bench.rs) exercise the
          # compiled binary end-to-end: they spawn it as a subprocess, fake
          # $HOME, probe localhost ports, and shell out to git — assumptions
          # the Nix build sandbox doesn't satisfy. CI runs the full suite
          # (`cargo test --locked`); the flake just builds.
          doCheck = false;

          meta = {
            description = cargoToml.package.description;
            homepage = cargoToml.package.repository;
            # Both, because Cargo.toml's expression is `MIT AND Apache-2.0`.
            # The Apache half is the terminal-UI code ported from OpenAI Codex
            # and xAI grok-build: NOTICE names every file it landed in, and
            # LICENSE-APACHE ships beside LICENSE so a `nix build` result
            # carries the text section 4(a) requires. Dropping either entry here
            # would make a package whose `meta.license` says less than the crate
            # it built.
            license = with lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "wizard";
          };
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };

        # The windowing libraries the native GUI `dlopen`s at run time, on the
        # platform that has them.
        #
        # Linux-only, because they are Linux-only: nixpkgs marks `wayland` and
        # the Xorg client libraries as unavailable for darwin, and winit on
        # macOS goes through AppKit and never looks for them. Listing them
        # unconditionally made `nix develop` and `nix flake check` fail outright
        # on macOS with "refusing to evaluate package 'wayland' … not available
        # on the requested hostPlatform" — a broken dev shell on one of the two
        # platforms this project supports, from an attribute that does nothing
        # there.
        guiLibs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
          pkgs.libxkbcommon
          pkgs.wayland
          pkgs.libx11
          pkgs.libxcursor
          pkgs.libxi
          pkgs.libxrandr
        ];
      in
      {
        packages = {
          default = pkgs.wizard;
          wizard = pkgs.wizard;
        };

        apps.default = {
          type = "app";
          program = "${pkgs.wizard}/bin/wizard";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ pkgs.wizard ];
          packages = [
            pkgs.rustc
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.nodejs
            pkgs.llama-cpp
          ];

          # The native GUI (`cargo build --features native`,
          # `wizard gui`) needs nothing at *build* time: `tiny-skia`
          # means no wgpu, and winit reaches X11 and Wayland through `dlopen`
          # rather than linking them, so there is no `-dev` package to find.
          #
          # `dlopen` is exactly why they have to be here anyway. On NixOS there
          # is no /usr/lib for the loader to fall back to, so a window opened
          # from this shell finds libX11 and libwayland-client only if
          # LD_LIBRARY_PATH names their store paths. The default package below
          # is unaffected: it builds with default features and links no iced,
          # which is the whole point of the feature flag. See `guiLibs` above
          # for why the list is empty on macOS.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = guiLibs;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath guiLibs;

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      }
    )
    // {
      overlays.default = final: prev: {
        wizard = mkWizard final;
      };

      # `homeModules` is the preferred name: `homeManagerModules` is not a
      # flake output Nix knows, so `nix flake check` reported it as unknown and
      # checked nothing inside it. Home Manager reads both, and this is the
      # name it settled on.
      #
      # The old name is kept as an alias rather than dropped, because it is
      # what every existing `inputs.wizard.homeManagerModules.default` import
      # names, and removing it turns an update into an evaluation error in
      # someone else's config. Nix still warns that it does not recognize the
      # attribute; that warning is the cost of not breaking those imports.
      homeManagerModules.default = self.homeModules.default;

      homeModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.wizard;
          tomlFormat = pkgs.formats.toml { };
        in
        {
          options.programs.wizard = {
            enable = lib.mkEnableOption "wizard, the sovereign self-extending agent";

            package = lib.mkOption {
              type = lib.types.package;
              default = mkWizard pkgs;
              defaultText = lib.literalExpression "wizard.packages.\${system}.default";
              description = "The wizard package to install.";
            };

            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              example = lib.literalExpression ''
                {
                  active_provider = "local";
                  providers = [
                    {
                      name = "local";
                      kind = "ollama";
                      base_url = "http://127.0.0.1:11434";
                      model = "qwen3:8b";
                    }
                  ];
                }
              '';
              description = ''
                Declarative wizard configuration written to
                {file}`~/.wizard/config.toml`. Leave empty to manage the
                config imperatively.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            home.file.".wizard/config.toml" = lib.mkIf (cfg.settings != { }) {
              source = tomlFormat.generate "wizard-config.toml" cfg.settings;
            };
          };
        };
    };
}
