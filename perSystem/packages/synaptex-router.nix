{...}: {
  perSystem = {
    craneLib,
    commonArgs,
    cargoArtifacts,
    lib,
    ...
  }: {
    packages.synaptex-router = craneLib.buildPackage (commonArgs // {
      inherit cargoArtifacts;
      pname = "synaptex-router";
      version = "0.1.0";
      cargoExtraArgs = "-p synaptex-router";
      meta = {
        description = "Synaptex router daemon — firewall, DHCP, and device discovery";
        mainProgram = "synaptex-router";
        license = lib.licenses.asl20;
      };
    });
  };
}
