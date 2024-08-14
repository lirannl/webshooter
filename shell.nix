{ pkgs ? import <nixpkgs> {} }:
  let
    #overrides = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml));
    libPath = with pkgs; lib.makeLibraryPath [
      # load external libraries that you need in your rust project here
    ];
in
 pkgs.mkShell rec {
    buildInputs = with pkgs; [
      clang
      llvmPackages.bintools
      rustup
      pkg-config
      openssl
      ffmpeg
      nodejs
      corepack
      pipewire
      dbus
      alsa-lib
      lldb
    ];
    #RUSTC_VERSION = overrides.toolchain.channel;
    # https://github.com/rust-lang/rust-bindgen#environment-variables
    LIBCLANG_PATH = pkgs.lib.makeLibraryPath [ pkgs.llvmPackages_latest.libclang.lib ];
    shellHook = ''
      export PATH=$PATH:''${CARGO_HOME:-~/.cargo}/bin
      export PATH=$PATH:''${RUSTUP_HOME:-~/.rustup}/toolchains/$RUSTC_VERSION-x86_64-unknown-linux-gnu/bin/
      '';
    
}
