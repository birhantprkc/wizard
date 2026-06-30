{
  description = "wizard — One line. Your sovereign agent. Self-extending. Fully local.";

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
      # The fusion-core git dependency from Cargo.lock. buildRustPackage's
      # cargoLock vendors crates.io crates by their lock hashes, but a git
      # dependency needs its fixed-output hash supplied explicitly, keyed by
      # "<name>-<version>".
      fusionCoreHash = "sha256-zzXJTpv4JOLt7ubP4gRZn23VHPrrW1bjeF4AH1FLJUA=";

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

          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "fusion-core-0.1.0" = fusionCoreHash;
            };
          };

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Put the optional runtime helpers on PATH so the default loadout
          # (Playwright MCP via npx, local models via llama-server/ollama)
          # works out of the box.
          postInstall = ''
            wrapProgram $out/bin/wizard \
              --prefix PATH : ${lib.makeBinPath runtimeBins}
          '';

          doCheck = false;

          meta = {
            description = cargoToml.package.description;
            homepage = cargoToml.package.repository;
            license = lib.licenses.mit;
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
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      }
    )
    // {
      overlays.default = final: prev: {
        wizard = mkWizard final;
      };

      homeManagerModules.default =
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
                  provider = "ollama";
                  model = "qwen3:0.6b";
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
