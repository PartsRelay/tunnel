#!/usr/bin/env bash
# Prints the manifest of a release from its six archives.
# Usage: make-manifest.sh VERSION TAG DIRECTORY > manifest.json
set -euo pipefail

version="${1:?usage: make-manifest.sh VERSION TAG DIRECTORY}"
tag="${2:?usage: make-manifest.sh VERSION TAG DIRECTORY}"
directory="${3:?usage: make-manifest.sh VERSION TAG DIRECTORY}"

targets=(
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
  x86_64-apple-darwin
  aarch64-apple-darwin
  x86_64-pc-windows-msvc
  aarch64-pc-windows-msvc
)

if [ "$tag" != "v$version" ]; then
  echo "the tag $tag is not v$version" >&2
  exit 1
fi

assets='{}'
for target in "${targets[@]}"; do
  case "$target" in
    *windows*) name="partsrelay-tunnel-$target.zip" ;;
    *) name="partsrelay-tunnel-$target.tar.gz" ;;
  esac
  file="$directory/$name"

  if [ ! -f "$file" ]; then
    echo "missing $file" >&2
    exit 1
  fi

  sha256="$(sha256sum "$file" | cut -d' ' -f1)"
  size="$(wc -c <"$file" | tr -d ' ')"
  assets="$(jq -c --arg target "$target" --arg name "$name" --arg sha256 "$sha256" \
    --argjson size "$size" '. + {($target): {name: $name, sha256: $sha256, size: $size}}' <<<"$assets")"
done

jq -n --arg version "$version" --arg tag "$tag" --argjson assets "$assets" \
  '{schema: 1, name: "partsrelay-tunnel", version: $version, tag: $tag, assets: $assets}'
