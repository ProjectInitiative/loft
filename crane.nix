# crane.nix
{ pkgs, craneLib, src, attic }:
let
  # Get the Nix libraries that attic needs without the cargo vendoring conflicts
  nixLibs = with pkgs; [
    nix    
    awscli2
    nlohmann_json
    boost
    brotli
    libsodium
    pkg-config
  ];
in
craneLib.buildPackage {
  inherit src;
  
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = nixLibs;
  
  # Ensure pkg-config can find the Nix libraries
  PKG_CONFIG_PATH = "${pkgs.nix}/lib/pkgconfig";
  
  # Prevent cargo config duplication by using our own vendoring
  cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };
}
