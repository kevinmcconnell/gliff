#!/usr/bin/env sh
# Compile the compute shaders to SPIR-V. The .spv files are committed so a
# build needs no shader compiler; rerun this after editing a .comp file.
set -e
cd "$(dirname "$0")"
for f in *.comp; do
    glslc -O --target-env=vulkan1.3 -o "${f%.comp}.spv" "$f"
done
