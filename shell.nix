let pkgs = import <nixpkgs> { };
in
pkgs.mkShell {
  name = "atuin";
  buildInputs = with pkgs; [
    rustup
    clang
    cargo-edit
  ];
}
