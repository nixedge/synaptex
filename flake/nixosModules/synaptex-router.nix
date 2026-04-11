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

          # ── Kea DHCP integration ──────────────────────────────────────────────

          keaSocket = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "/run/synaptex-router/kea-hook.sock";
            description = ''
              Unix socket path for the Kea hook shim to connect to.
              When set, the daemon classifies DHCP packets forwarded by the shim.
              Must match the "socket" parameter in the kea-dhcp4.conf hooks-libraries entry.
              Requires keaIotRelay to also be set.
            '';
          };

          keaIotRelay = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            example = ["10.40.8.1"];
            description = ''
              Relay agent IP(s) for the IoT VLAN (giaddr values).
              Only DHCP requests arriving via these relay IPs are classified as IOT_DEVICE.
            '';
          };

          keaCtrlSocket = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "/run/kea/kea-dhcp4.sock";
            description = ''
              Unix socket path for the Kea DHCPv4 control channel.
              Requires the host_cmds hook in kea-dhcp4.conf.
              When set, device reservations are pushed to Kea on discovery
              and re-synced from the router DB at startup.
            '';
          };

          keaSubnetId = lib.mkOption {
            type = lib.types.int;
            default = 0;
            example = 10408;
            description = ''
              Kea DHCPv4 subnet-id for managed reservations.
              Must match the subnet-id in kea-dhcp4.conf for the IoT VLAN subnet.
            '';
          };

          # ── Managed IP allocation ─────────────────────────────────────────────

          managedSubnet = lib.mkOption {
            type = lib.types.str;
            default = "10.40.8";
            example = "192.168.10";
            description = ''
              First three octets of the subnet synaptex manages IP allocation within.
              Synaptex allocates addresses between managedHostStart and managedHostEnd
              and pushes them to Kea as reservations.
            '';
          };

          managedHostStart = lib.mkOption {
            type = lib.types.int;
            default = 21;
            description = ''
              First host octet synaptex may allocate within the managed subnet.
            '';
          };

          managedHostEnd = lib.mkOption {
            type = lib.types.int;
            default = 223;
            description = ''
              Last host octet synaptex may allocate within the managed subnet (inclusive).
            '';
          };

          managedLeaseSecs = lib.mkOption {
            type = lib.types.nullOr lib.types.int;
            default = null;
            example = 86400;
            description = ''
              DHCP valid lifetime (seconds) for managed (reserved) devices.
              Also sets T1 = lease÷2 and T2 = lease×7÷8 via per-reservation
              option-data, overriding the subnet-level renew/rebind timers.
              Leave null to inherit subnet defaults.
            '';
          };

          openFirewall = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Open the gRPC port in the firewall.";
          };

          keaHookSrc = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            example = "inputs.synaptex + \"/src/kea-hook\"";
            description = ''
              Path to the synaptex kea-hook source tree (the directory containing
              CMakeLists.txt).  When set, the hook is compiled against the
              system's kea package and injected into kea's store path so Kea's
              hook library security check accepts it.

              Set this to <literal>inputs.synaptex + "/src/kea-hook"</literal>
              in your configuration.  The hook must be compiled against the
              same kea version as the running binary (KEA_HOOKS_VERSION must
              match), which this overlay guarantees by using prev.kea as the
              build input.

              Leave null to skip the overlay (hook not installed).
            '';
          };
        };

        config = lib.mkIf cfg.enable {
          assertions = [
            {
              assertion = (cfg.certFile == null) == (cfg.keyFile == null);
              message = "services.synaptex-router: certFile and keyFile must both be set or both be null.";
            }
            {
              assertion = cfg.keaSocket == null || cfg.keaIotRelay != [];
              message = "services.synaptex-router: keaIotRelay must be set when keaSocket is configured.";
            }
          ];

          networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [50052];

          # Bundle synaptex_hook.so into the consumer's kea store path when
          # keaHookSrc is set.  Kea 2.5+ validates that hook libraries reside
          # under its own installation prefix (a compile-time constant).
          # Building against prev.kea guarantees KEA_HOOKS_VERSION matches.
          nixpkgs.overlays = lib.mkIf (cfg.keaHookSrc != null) [
            (_final: prev: {
              kea = prev.kea.overrideAttrs (old: let
                keaHook = prev.stdenv.mkDerivation {
                  pname = "synaptex-kea-hook";
                  version = "0.1.0";
                  # builtins.path produces a content-addressed store path whose
                  # hash depends only on the directory contents, not the source
                  # flake's store path.  Without this, any synaptex commit would
                  # change cfg.keaHookSrc's store prefix and trigger a kea rebuild
                  # even when the kea-hook files are unchanged.
                  src = builtins.path {
                    name = "synaptex-kea-hook-src";
                    path = cfg.keaHookSrc;
                  };
                  nativeBuildInputs = [prev.cmake];
                  buildInputs = [prev.kea prev.boost];
                  cmakeFlags = ["-DKEA_INCLUDE_DIR=${prev.kea}/include/kea"];
                  installPhase = ''
                    runHook preInstall
                    install -Dm755 synaptex_hook.so \
                      $out/lib/kea/hooks/synaptex_hook.so
                    runHook postInstall
                  '';
                };
              in {
                postInstall =
                  (old.postInstall or "")
                  + ''
                    install -Dm755 ${keaHook}/lib/kea/hooks/synaptex_hook.so \
                      $out/lib/kea/hooks/synaptex_hook.so
                  '';
              });
            })
          ];

          # Ensure a static "kea" group exists so the socket ownership below
          # resolves to a named group rather than a transient DynamicUser GID.
          users.groups.kea = lib.mkIf (cfg.keaCtrlSocket != null || cfg.keaSocket != null) {};

          # Kea uses DynamicUser = true, so its control socket is owned by a
          # transient UID/GID that appears as "nobody/nogroup".  Override the
          # kea-dhcp4-server service to use a static "kea" group and set the
          # runtime directory / umask so the socket is group-accessible.
          # synaptex-router's SupplementaryGroups = ["kea"] then grants access.
          systemd.services.kea-dhcp4-server = lib.mkIf (cfg.keaCtrlSocket != null) {
            serviceConfig = {
              Group = lib.mkForce "kea";
              RuntimeDirectoryMode = lib.mkForce "0750";
              UMask = lib.mkForce "0007";
            };
          };

          systemd.services.synaptex-router = {
            description = "Synaptex router daemon";
            wantedBy = ["multi-user.target"];
            after = ["network.target" "kea-dhcp4.service"];

            serviceConfig = let
              stateDir = "/var/lib/synaptex-router";
            in {
              ExecStart = lib.concatStringsSep " " (
                [
                  "${cfg.package}/bin/synaptex-router"
                  "--listen ${cfg.listenAddress}"
                  "--db ${cfg.dbPath}"
                  "--managed-subnet ${cfg.managedSubnet}"
                  "--managed-host-start ${toString cfg.managedHostStart}"
                  "--managed-host-end ${toString cfg.managedHostEnd}"
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
                ++ lib.optionals (cfg.keaSocket != null) [
                  "--kea-socket ${cfg.keaSocket}"
                  "--kea-iot-relay ${lib.concatStringsSep "," cfg.keaIotRelay}"
                ]
                ++ lib.optionals (cfg.keaCtrlSocket != null) [
                  "--kea-ctrl-socket ${cfg.keaCtrlSocket}"
                  "--kea-subnet-id ${toString cfg.keaSubnetId}"
                ]
                ++ lib.optionals (cfg.managedLeaseSecs != null) [
                  "--managed-lease-secs ${toString cfg.managedLeaseSecs}"
                ]
              );

              DynamicUser = true;
              StateDirectory = "synaptex-router";
              StateDirectoryMode = "0750";
              # Socket for the Kea hook shim lives here.
              RuntimeDirectory = "synaptex-router";
              RuntimeDirectoryMode = "0755";
              # Needs access to the Kea control socket (owned by kea group).
              SupplementaryGroups = ["kea"];

              Restart = "on-failure";
              RestartSec = "5s";

              # Needs CAP_NET_RAW for SO_BINDTODEVICE and raw UDP.
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
              # AF_UNIX needed for Kea control socket and hook socket.
              RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
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
