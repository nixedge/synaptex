{...}: {
  perSystem = {
    pkgs,
    lib,
    ...
  }: {
    packages.kea-hook = pkgs.stdenv.mkDerivation {
      pname    = "synaptex-kea-hook";
      version  = "0.1.0";
      src      = ../../src/kea-hook;

      nativeBuildInputs = [ pkgs.cmake ];
      buildInputs       = [ pkgs.kea pkgs.boost ];

      cmakeFlags = [
        "-DKEA_INCLUDE_DIR=${pkgs.kea}/include/kea"
      ];

      installPhase = ''
        runHook preInstall
        install -Dm755 synaptex_hook.so $out/lib/kea/hooks/synaptex_hook.so
        runHook postInstall
      '';

      meta = {
        description = "Kea pkt4_receive hook shim for synaptex-router classification";
        license     = lib.licenses.asl20;
      };
    };
  };
}
