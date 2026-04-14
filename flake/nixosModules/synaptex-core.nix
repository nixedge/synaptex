{
  flake = {config, ...}: {
    nixosModules = {
      synaptex-core = {
        lib,
        pkgs,
        config,
        ...
      }: let
        cfg = config.services.synaptex-core;
      in {
        options.services.synaptex-core = {
          enable = lib.mkEnableOption "Synaptex smart home controller daemon";

          package = lib.mkPackageOption pkgs "synaptex-core" {};

          httpPort = lib.mkOption {
            type = lib.types.port;
            default = 8080;
            description = "Port for the HTTP/REST API.";
          };

          dbPath = lib.mkOption {
            type = lib.types.path;
            default = "/var/lib/synaptex-core/db";
            description = "Directory for the sled database.";
          };

          routerUrl = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "https://192.168.10.1:50052";
            description = "gRPC URL of the synaptex-router daemon. Requires routerCertFile.";
          };

          routerCertFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = "Path to synaptex-router's TLS certificate PEM (for mTLS).";
          };

          openFirewall = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Open the HTTP API port in the firewall.";
          };

          environmentFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              Path to an environment file (EnvironmentFile) for passing secrets such as
              SYNAPTEX_TUYA_CLIENT_SECRET or a pre-set SYNAPTEX_API_KEY.
            '';
          };
        };

        config = lib.mkIf cfg.enable {
          assertions = [
            {
              assertion = (cfg.routerUrl == null) == (cfg.routerCertFile == null);
              message = "services.synaptex-core: routerUrl and routerCertFile must both be set or both be null.";
            }
          ];

          networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [cfg.httpPort];

          systemd.services.synaptex-core = {
            description = "Synaptex smart home controller daemon";
            wantedBy = ["multi-user.target"];
            after = ["network.target"];

            serviceConfig = {
              ExecStart = lib.concatStringsSep " " (
                [
                  "${cfg.package}/bin/synaptex-core"
                  "--db ${cfg.dbPath}"
                  "--http-port ${toString cfg.httpPort}"
                ]
                ++ lib.optionals (cfg.routerUrl != null) [
                  "--router-url ${cfg.routerUrl}"
                  "--router-cert ${cfg.routerCertFile}"
                ]
              );

              EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;

              User = "synaptex-core";
              Group = "synaptex-core";
              StateDirectory = "synaptex-core";
              StateDirectoryMode = "0750";

              Restart = "on-failure";
              RestartSec = "5s";

              # Hardening
              NoNewPrivileges = true;
              ProtectSystem = "strict";
              ProtectHome = true;
              PrivateTmp = true;
              PrivateDevices = true;
              ProtectKernelTunables = true;
              ProtectKernelModules = true;
              ProtectControlGroups = true;
              RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
              LockPersonality = true;
              MemoryDenyWriteExecute = true;
              RestrictRealtime = true;
              SystemCallFilter = ["@system-service"];
            };
          };
          users.users.synaptex-core = {
            isSystemUser = true;
            group = "synaptex-core";
          };
          users.groups.synaptex-core = {};
        };
      };
    };
  };
}
