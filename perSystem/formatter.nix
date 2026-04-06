{
  perSystem = {
    config,
    pkgs,
    ...
  }: {
    treefmt = {
      projectRootFile = "flake.nix";

      programs.alejandra.enable = true;
      programs.rustfmt.enable = true;
      programs.taplo.enable = true;

      settings.global.excludes = [
        "*.lock"
        "*.patch"
        ".gitattributes"
        ".gitignore"
        ".gitmodules"
        "LICENSE"
      ];
    };

    formatter = config.treefmt.build.wrapper;
  };
}
