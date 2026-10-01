{
  description = "busybee VM lab: NixOS worker template";

  # Pinned to the revision of the installer ISO in linux/installer.json, so the
  # live installer and the installed system come from the same nixpkgs.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/78e9c786dc08cd4f3420c2395cd977206a9b1da2";

  outputs = { nixpkgs, ... }: {
    nixosConfigurations.linux-aarch64 = nixpkgs.lib.nixosSystem {
      system = "aarch64-linux";
      modules = [ ./linux/configuration.nix ];
    };
  };
}
