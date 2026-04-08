{
  flake = {config, ...}: {
    nixosModules = {
      synaptex-router = {
        lib,
        pkgs,
        config,
        ...
      }: let
        cfg = config.services.synaptex-router;
      in {
        options.services.synaptex-router = {
          enable = lib.mkEnableOption "Synaptex router daemon (firewall, DHCP, device discovery)";

          package = lib.mkPackageOption pkgs "synaptex-router" {};

          listenAddress = lib.mkOption {
            type = lib.types.str;
            default = "[::]:50052";
            description = "Address for the gRPC service to listen on.";
          };

          certFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              Path to the TLS certificate PEM. If null, a self-signed certificate is
              auto-generated on first run and written to the state directory.
            '';
          };

          keyFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              Path to the TLS private key PEM. Must be set together with certFile.
            '';
          };

          clientCaFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              Path to the client CA certificate PEM for mTLS. When set, only clients
              presenting a certificate signed by this CA are accepted (e.g. synaptex-core's cert).
            '';
          };

          dbPath = lib.mkOption {
            type = lib.types.path;
            default = "/var/lib/synaptex-router/db";
            description = "Directory for the sled database (discovered device records).";
          };

          interfaces = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            example = ["br-iot" "br-lan"];
            description = ''
              Network interfaces to bind for Tuya UDP discovery (ports 6666/6667).
              Empty list binds on all interfaces.
            '';
          };

          openFirewall = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Open the gRPC port in the firewall.";
          };
        };

        config = lib.mkIf cfg.enable {
          assertions = [
            {
              assertion = (cfg.certFile == null) == (cfg.keyFile == null);
              message = "services.synaptex-router: certFile and keyFile must both be set or both be null.";
            }
          ];

          networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [50052];

          systemd.services.synaptex-router = {
            description = "Synaptex router daemon";
            wantedBy = ["multi-user.target"];
            after = ["network.target"];

            serviceConfig = let
              stateDir = "/var/lib/synaptex-router";
            in {
              ExecStart = lib.concatStringsSep " " (
                [
                  "${cfg.package}/bin/synaptex-router"
                  "--listen ${cfg.listenAddress}"
                  "--db ${cfg.dbPath}"
                ]
                ++ lib.optionals (cfg.certFile != null) [
                  "--cert ${cfg.certFile}"
                  "--key ${cfg.keyFile}"
                ]
                ++ lib.optionals (cfg.certFile == null) [
                  "--cert ${stateDir}/router.crt"
                  "--key ${stateDir}/router.key"
                ]
                ++ lib.optionals (cfg.clientCaFile != null) [
                  "--client-ca ${cfg.clientCaFile}"
                ]
                ++ lib.optionals (cfg.interfaces != []) [
                  "--interfaces ${lib.concatStringsSep "," cfg.interfaces}"
                ]
              );

              DynamicUser = true;
              StateDirectory = "synaptex-router";
              StateDirectoryMode = "0750";

              Restart = "on-failure";
              RestartSec = "5s";

              # Needs CAP_NET_RAW for SO_BINDTODEVICE and raw UDP
              AmbientCapabilities = ["CAP_NET_RAW"];
              CapabilityBoundingSet = ["CAP_NET_RAW"];

              # Hardening
              NoNewPrivileges = true;
              ProtectSystem = "strict";
              ProtectHome = true;
              PrivateTmp = true;
              ProtectKernelTunables = true;
              ProtectKernelModules = true;
              ProtectControlGroups = true;
              RestrictAddressFamilies = ["AF_INET" "AF_INET6"];
              LockPersonality = true;
              RestrictRealtime = true;
              SystemCallFilter = ["@system-service" "@network-io"];
            };
          };
        };
      };
    };
  };
}
