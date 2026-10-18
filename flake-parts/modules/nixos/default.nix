{
  config,
  inputs,
  self,
  ...
}:
let
  inherit (inputs.flake-parts.lib) importApply;
in
{
  config.flake.nixosModules = {
    ccdash = ./ccdash.nix;
    hub = importApply ./hub.nix { localFlake = self; };
    default = config.flake.nixosModules.ccdash;
  };
}
