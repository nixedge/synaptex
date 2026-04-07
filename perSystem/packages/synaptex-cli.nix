{...}: {
  perSystem = {
    craneLib,
    commonArgs,
    cargoArtifacts,
    lib,
    ...
  }: {
    packages.synaptex-cli = craneLib.buildPackage (commonArgs // {
      inherit cargoArtifacts;
      pname = "synaptex-cli";
      version = "0.1.0";
      cargoExtraArgs = "-p synaptex-cli";
      meta = {
        description = "Synaptex CLI client";
        mainProgram = "synaptex-cli";
        license = lib.licenses.asl20;
      };
    });
  };
}
