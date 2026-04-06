{
  flake = {config, ...}: {
    nixosModules = let
      name = "nixos-module-name";
    in {
      default = {
        imports = [config.nixosModules.${name}];
      };

      ${name} = {
        config,
        lib,
        pkgs,
        ...
      }: let
        cfg = config.services.${name};
      in {
        options.services.${name} = {
          enable = lib.mkEnableOption "description";
          package = lib.mkPackageOption pkgs name {};
        };

        config =
          lib.mkIf cfg.enable {
          };
      };
    };
  };
}
