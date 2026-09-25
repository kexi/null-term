#!/usr/bin/env bash
# trunk の post_build フック: ライセンス表記を配信物に入れる
#   LICENSE.txt                 null-term 本体 (MIT)
#   THIRD-PARTY-LICENSES.html   wasm に組み込まれる依存クレート (リリースビルドのみ)
# 拡張子なしの LICENSE は GitHub Pages がダウンロード扱いにするので .txt にする
set -euo pipefail

cd "$TRUNK_SOURCE_DIR"
cp ../../LICENSE "$TRUNK_STAGING_DIR/LICENSE.txt"

# cargo-about は数秒かかるので、trunk serve の再ビルドごとには作らない
if [[ "${TRUNK_PROFILE:-}" == "release" ]]; then
    cargo about generate about.hbs -o "$TRUNK_STAGING_DIR/THIRD-PARTY-LICENSES.html"
fi
