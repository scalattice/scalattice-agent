#!/usr/bin/env bash
# Bundle non-glibc shared libraries so the agent runs without apt install steps.
set -euo pipefail

BINARY="${1:?binary path}"
OUT_DIR="${2:?output directory}"
BUILD_ROOT="${3:-$(dirname "$BINARY")}"
LIB_DIR="$OUT_DIR/lib"
BACKENDS_DIR="$LIB_DIR/backends"
mkdir -p "$LIB_DIR"

# glibc + base toolchain: always on Linux; never bundle.
SKIP_RE='/(libc\.so|libm\.so|libpthread|libdl\.so|librt\.so|libresolv\.so|libstdc\+\+|libgcc_s|ld-linux)'

# NVIDIA user-space driver: must come from the host GPU driver, not our tarball.
# (Never ship a stub here — rpath would shadow the real driver on NVIDIA hosts.)
SKIP_RE="${SKIP_RE}|libcuda\.so|libnvidia"

SEARCH_ROOT="$(dirname "$BUILD_ROOT")"

bundle_from() {
  local bin="$1"
  # Resolve against already-bundled libs + build outputs (dynamic-link ggml/llama).
  local ld_path="$LIB_DIR"
  [[ -d "$BACKENDS_DIR" ]] && ld_path="$BACKENDS_DIR:$ld_path"
  local build_lib
  while IFS= read -r build_lib; do
    [[ -n "$build_lib" ]] || continue
    ld_path="$build_lib:$ld_path"
  done < <(find "$SEARCH_ROOT" -type d \( -path '*/llama-cpp-sys-2-*/out/lib' -o -path '*/llama-cpp-sys-2-*/out/build/bin' \) 2>/dev/null | head -20 || true)

  LD_LIBRARY_PATH="$ld_path${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    ldd "$bin" 2>/dev/null | awk '/=> \// {print $3}' | while read -r lib; do
    [[ -f "$lib" ]] || continue
    echo "$lib" | grep -Eq "$SKIP_RE" && continue
    local dest="$LIB_DIR/$(basename "$lib")"
    # Same inode (already bundled) — skip; cp -L errors on identical paths under set -e.
    if [[ -e "$dest" ]] && [[ "$lib" -ef "$dest" ]]; then
      continue
    fi
    cp -L "$lib" "$dest"
  done
}

copy_named_libs() {
  local root="$1"
  [[ -d "$root" ]] || return 0
  # Preserve SONAME symlinks (libfoo.so.0 -> libfoo.so.0.x.y); do not dereference.
  find "$root" -maxdepth 1 \( -type f -o -type l \) \( \
      -name 'libggml.so*' -o -name 'libggml-base.so*' -o -name 'libllama.so*' -o -name 'libllama-common.so*' \
    \) 2>/dev/null | while read -r lib; do
      local base dest
      base="$(basename "$lib")"
      dest="$LIB_DIR/$base"
      if [[ -e "$dest" ]] || [[ -L "$dest" ]]; then
        if [[ "$lib" -ef "$dest" ]]; then
          continue
        fi
      fi
      cp -a "$lib" "$dest"
    done || true
}

copy_backend_modules() {
  local root="$1"
  [[ -d "$root" ]] || return 0
  find "$root" -maxdepth 1 -type f \( \
      -name 'libggml-*.so*' -o -name 'libggml_*.so*' -o \
      -name 'ggml-*.so*' -o -name 'ggml_*.so*' \
    \) 2>/dev/null | while read -r lib; do
      local base
      base="$(basename "$lib")"
      # Core libs are not backends (libggml.so / libggml-base.so handled above).
      case "$base" in
        libggml.so*|libggml-base.so*) continue ;;
      esac
      mkdir -p "$BACKENDS_DIR"
      local dest="$BACKENDS_DIR/$base"
      if [[ -e "$dest" ]] && [[ "$lib" -ef "$dest" ]]; then
        continue
      fi
      cp -L "$lib" "$dest"
    done || true
}

# 1) Core shared llama/ggml from the crate OUT_DIR (dynamic-link).
while IFS= read -r dir; do
  [[ -n "$dir" ]] || continue
  copy_named_libs "$dir"
done < <(find "$SEARCH_ROOT" -type d \( -path '*/llama-cpp-sys-2-*/out/lib' -o -path '*/llama-cpp-sys-2-*/out/build/bin' -o -path '*/llama-cpp-sys-2-*/out/build/lib' \) 2>/dev/null || true)

# 2) Backend plugins (CUDA / Vulkan / CPU variants).
while IFS= read -r dir; do
  [[ -n "$dir" ]] || continue
  copy_backend_modules "$dir"
done < <(find "$SEARCH_ROOT" -type d \( -name backends -o -path '*/llama-cpp-sys-2-*/out/build/bin' \) 2>/dev/null || true)

# 2b) CUDA redistributable runtime (cudart/cublas) for libggml-cuda.so.
# NVIDIA allows shipping these; libcuda.so stays host-driver-only.
# Without them, dynamic-backends NVIDIA machines that only installed the
# driver (no toolkit) would fail to dlopen the CUDA module.
copy_cuda_redist() {
  local roots=()
  [[ -n "${CUDA_PATH:-}" ]] && roots+=("$CUDA_PATH")
  [[ -n "${CUDA_HOME:-}" ]] && roots+=("$CUDA_HOME")
  roots+=(/usr/local/cuda /usr/local/cuda-12.6 /usr/local/cuda-12)
  local root libdir
  for root in "${roots[@]}"; do
    for libdir in "$root/lib64" "$root/lib" "$root/targets/x86_64-linux/lib" "$root/targets/aarch64-linux/lib"; do
      [[ -d "$libdir" ]] || continue
      local copied=0
      for pattern in \
        'libcudart.so.12*' 'libcudart.so' \
        'libcublas.so.12*' 'libcublas.so' \
        'libcublasLt.so.12*' 'libcublasLt.so'
      do
        for lib in "$libdir"/$pattern; do
          [[ -e "$lib" ]] || continue
          cp -a "$lib" "$LIB_DIR/$(basename "$lib")"
          copied=1
        done
      done
      [[ "$copied" -eq 1 ]] && return 0
    done
  done
  echo "warning: CUDA cudart/cublas redist not found (NVIDIA hosts need them to load ggml-cuda)" >&2
}
copy_cuda_redist

# 3) Transitive deps of the binary + core libs (skip chasing CUDA driver).
bundle_from "$BINARY"
for _ in 1 2 3 4 5; do
  before="$(find "$LIB_DIR" -type f | wc -l)"
  for lib in "$LIB_DIR"/*; do
    [[ -f "$lib" ]] || continue
    bundle_from "$lib"
  done
  for lib in "$BACKENDS_DIR"/*; do
    [[ -f "$lib" ]] || continue
    case "$(basename "$lib")" in
      *cuda*|*Cuda*|*CUDA*) continue ;;
    esac
    bundle_from "$lib"
  done
  after="$(find "$LIB_DIR" -type f | wc -l)"
  [[ "$after" -le "$before" ]] && break
done

if command -v patchelf >/dev/null 2>&1; then
  # Install layout: ~/.local/bin/scalattice-agent → ~/.local/lib/scalattice
  patchelf --set-rpath '$ORIGIN/../lib/scalattice' "$BINARY"
  for lib in "$LIB_DIR"/*.so*; do
    [[ -f "$lib" ]] || continue
    patchelf --set-rpath '$ORIGIN' "$lib" 2>/dev/null || true
  done
  for lib in "$BACKENDS_DIR"/*.so*; do
    [[ -f "$lib" ]] || continue
    # Backends resolve siblings + parent lib dir (libggml-base, OpenMP, …).
    patchelf --set-rpath '$ORIGIN:$ORIGIN/..' "$lib" 2>/dev/null || true
  done
fi

if [[ -z "$(ls -A "$BACKENDS_DIR" 2>/dev/null || true)" ]]; then
  rmdir "$BACKENDS_DIR" 2>/dev/null || true
fi

if [[ -z "$(ls -A "$LIB_DIR" 2>/dev/null || true)" ]]; then
  rmdir "$LIB_DIR" 2>/dev/null || true
fi
