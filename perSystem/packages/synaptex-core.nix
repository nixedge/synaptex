{...}: {
  perSystem = {
    craneLib,
    commonArgs,
    cargoArtifacts,
    lib,
    ...
  }: {
    packages.synaptex-core = craneLib.buildPackage (commonArgs // {
      inherit cargoArtifacts;
      pname = "synaptex-core";
      version = "0.1.0";
      cargoExtraArgs = "-p synaptex-core";
      meta = {
        description = "Synaptex smart home controller daemon";
        mainProgram = "synaptex-core";
        license = lib.licenses.asl20;
      };
    });
  };
}
