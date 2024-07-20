{
 description = "example-node-js-flake";
 inputs = {
  flake-utils.url = "github:numtide/flake-utils";
 };
 outputs = {nixpkgs, flake-utils, ...}:
 flake-utils.lib.eachDefaultSystem (system:
 let
  pkgs = import nixpkgs {
   inherit system;
  };
 in {
  flakedPkgs = pkgs;
  devShell = pkgs.mkShell {
  buildInputs = with pkgs; [
    nodejs
  ];};
  }
 );
}
