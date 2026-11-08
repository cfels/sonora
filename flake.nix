{
  description = "Sonora desktop development environment";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { nixpkgs, ... }: {
    devShells = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: {
      default = import ./nix/dev-shell.nix { pkgs = nixpkgs.legacyPackages.${system}; };
    });
  };
}
