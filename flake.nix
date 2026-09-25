{
  description = "null-term: パソコン通信用 2 画面シリアルターミナル";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            # ブラウザ版のビルドに wasm32 ターゲットが要る
            (pkgs.rust-bin.stable.latest.default.override { targets = [ "wasm32-unknown-unknown" ]; })
            # wasm-bindgen は Cargo.lock と同じ版を trunk が自動で取ってくる
            pkgs.trunk
            # ブラウザ版に同梱する依存クレートのライセンス一覧を作る
            pkgs.cargo-about
          ];
        };
      });
    };
}
