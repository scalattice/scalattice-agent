#!/usr/bin/env bash
# Build scalattice-sd from leejet/stable-diffusion.cpp and install it under
# native/sd/out. The agent spawns this binary on the slot the server picked.
# Its ggml lives in native/sd/out/lib, separate from llama.cpp's ggml, so the
# two do not collide and the agent binary does not DT_NEED libcuda.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${ROOT}/native/sd/upstream"
BUILD="${SRC}/build"
OUT="${ROOT}/native/sd/out"
WRAPPER="${ROOT}/native/sd/scalattice_sd.cpp"
# Pin: scripts/patches/sd-load-cuda-on-demand.patch is authored against this tip.
# Bumping requires refreshing that patch (and plugin_entry.cpp) first.
SD_UPSTREAM_PIN="${SD_UPSTREAM_PIN:-f5df5f8b84f139f1e80496902da90798a0d92896}"

mkdir -p "${ROOT}/native/sd"
if [[ ! -d "${SRC}/.git" ]]; then
  git clone --recursive https://github.com/leejet/stable-diffusion.cpp "${SRC}"
fi
# Detached pin so CI does not silently drift past our CUDA-on-demand patch.
git -C "${SRC}" fetch --depth 1 origin "${SD_UPSTREAM_PIN}" 2>/dev/null || \
  git -C "${SRC}" fetch --depth 50 origin master
git -C "${SRC}" checkout --detach -f "${SD_UPSTREAM_PIN}"
# ggml is a submodule. A previous clone without --recursive leaves the directory empty.
if [[ ! -f "${SRC}/ggml/CMakeLists.txt" ]]; then
  git -C "${SRC}" submodule update --init --recursive --depth 1
fi
# CUDA kernels live in libsd-cuda.so and are opened only on NVIDIA machines.
# Without this patch, libstable-diffusion.so DT_NEEDs libcuda and AMD/Intel hosts
# cannot start the image runner — fail the build rather than ship a broken binary.
patch_file="${ROOT}/scripts/patches/sd-load-cuda-on-demand.patch"
if [[ ! -f "${patch_file}" ]]; then
  echo "missing ${patch_file}" >&2
  exit 1
fi
if git -C "${SRC}" apply --reverse --check "${patch_file}" >/dev/null 2>&1; then
  echo "==> sd CUDA-on-demand patch already applied"
elif git -C "${SRC}" apply --check "${patch_file}" >/dev/null 2>&1; then
  git -C "${SRC}" apply "${patch_file}"
  echo "==> applied sd CUDA-on-demand patch"
else
  echo "sd CUDA-on-demand patch does not apply at ${SD_UPSTREAM_PIN}; refresh the patch" >&2
  git -C "${SRC}" apply --check "${patch_file}" || true
  exit 1
fi
if [[ ! -f "${SRC}/src/core/cuda/plugin_entry.cpp" ]]; then
  echo "plugin_entry.cpp missing after patch; CUDA plugin cannot register" >&2
  exit 1
fi

cmake_args=(
  -S "${SRC}"
  -B "${BUILD}"
  -DCMAKE_BUILD_TYPE=Release
  -DSD_BUILD_EXAMPLES=OFF
  -DSD_BUILD_SHARED_LIBS=ON
  -DSD_BUILD_SHARED_GGML_LIB=ON
  -DBUILD_SHARED_LIBS=ON
  -DGGML_NATIVE=OFF
  -DGGML_BACKEND_DL=ON
)

case "$(uname -s)" in
  Darwin)
    cmake_args+=(-DSD_METAL=ON -DSD_CUDA=OFF -DSD_VULKAN=OFF)
    ;;
  *)
    cmake_args+=(-DSD_CUDA=ON -DSD_VULKAN=ON -DSD_METAL=OFF)
    if [[ -z "${CUDA_PATH:-}" ]]; then
      if [[ -d /usr/local/cuda ]]; then
        export CUDA_PATH=/usr/local/cuda
      elif [[ -d /usr/local/cuda-12.6 ]]; then
        export CUDA_PATH=/usr/local/cuda-12.6
      fi
    fi
    if [[ -n "${CUDA_PATH:-}" ]]; then
      export PATH="${CUDA_PATH}/bin:${PATH}"
    fi
    ;;
esac

echo "==> cmake ${cmake_args[*]}"
cmake "${cmake_args[@]}"
cmake --build "${BUILD}" -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu)"

lib_dir=""
for candidate in \
  "${BUILD}/bin" \
  "${BUILD}/lib" \
  "${BUILD}"
do
  if compgen -G "${candidate}/libstable-diffusion.so*" >/dev/null || \
     compgen -G "${candidate}/libstable-diffusion.dylib" >/dev/null; then
    lib_dir="${candidate}"
    break
  fi
done
if [[ -z "${lib_dir}" ]]; then
  echo "libstable-diffusion was not produced. Looked under ${BUILD}" >&2
  find "${BUILD}" -name 'libstable-diffusion.*' -o -name 'stable-diffusion.lib' | head
  exit 1
fi

rm -rf "${OUT}"
mkdir -p "${OUT}/lib"
shopt -s nullglob
# Core shared libs next to the runner (Linux .so* / macOS .dylib).
# Use globs only — a literal missing path would trip `set -e` under Darwin.
for lib in "${lib_dir}"/libstable-diffusion.* \
           "${lib_dir}"/libsd-cuda.* \
           "${lib_dir}"/libggml.* \
           "${lib_dir}"/libggml-base.*; do
  case "$(basename "${lib}")" in
    libggml-cuda.*|libggml-vulkan.*|libggml-metal.*|libggml-cpu.*|libggml-blas.*)
      # Backends ship beside the executable, not under lib/.
      continue
      ;;
  esac
  cp -a "${lib}" "${OUT}/lib/"
done
# ggml opens CUDA/Vulkan/Metal/CPU backends from the executable directory,
# and only when that GPU exists. Keep them out of lib/ and out of the
# libstable-diffusion link line.
for lib in "${lib_dir}"/libggml-cuda.* "${lib_dir}"/libggml-vulkan.* \
           "${lib_dir}"/libggml-metal.* "${lib_dir}"/libggml-cpu.* \
           "${lib_dir}"/libggml-blas.* \
           "${BUILD}/bin"/libggml-cuda.* "${BUILD}/bin"/libggml-vulkan.* \
           "${BUILD}/bin"/libggml-metal.* "${BUILD}/bin"/libggml-cpu.* \
           "${BUILD}/bin"/libggml-blas.*; do
  [[ -e "${lib}" ]] || continue
  cp -a "${lib}" "${OUT}/"
done
shopt -u nullglob
if [[ ! -f "${OUT}/lib/libsd-cuda.so" && -f "${BUILD}/bin/libsd-cuda.so" ]]; then
  cp -a "${BUILD}/bin/libsd-cuda.so" "${OUT}/lib/"
fi

cxx="c++"
if command -v clang++ >/dev/null 2>&1; then
  cxx="clang++"
fi
echo "==> ${cxx} scalattice_sd.cpp"
# libstable-diffusion leaves ggml symbols in libggml / libggml-base. Link them
# explicitly; modern ld does not pull a shared library's DT_NEEDED for us.
link_args=(
  -O2 -std=c++17 -I"${SRC}/include" "${WRAPPER}"
  -L"${OUT}/lib"
  -lstable-diffusion -lggml -lggml-base
)
stub_link=""
case "$(uname -s)" in
  Darwin)
    link_args+=(-Wl,-rpath,@loader_path/lib)
    ;;
  *)
    link_args+=(-Wl,-rpath-link,"${OUT}/lib" -Wl,-rpath,'$ORIGIN/lib')
    # GPU-less CI: libsd-cuda (and a mis-patched libstable-diffusion) may
    # DT_NEED libcuda.so.1. Resolve symbols against the toolkit stub only —
    # never bake the stubs dir into the runtime rpath.
    stub_dir=""
    for candidate in \
      "${CUDA_PATH:-}/lib64/stubs" \
      /usr/local/cuda/lib64/stubs \
      /usr/local/cuda-12.6/lib64/stubs
    do
      if [[ -f "${candidate}/libcuda.so" ]]; then
        stub_dir="${candidate}"
        break
      fi
    done
    if [[ -n "${stub_dir}" ]]; then
      stub_link="$(mktemp -d)"
      ln -s "${stub_dir}/libcuda.so" "${stub_link}/libcuda.so.1"
      link_args+=(-Wl,-rpath-link,"${stub_link}")
    fi
    ;;
esac
"${cxx}" "${link_args[@]}" -o "${OUT}/scalattice-sd"
if [[ -n "${stub_link}" ]]; then
  rm -rf "${stub_link}"
fi
chmod +x "${OUT}/scalattice-sd"
if command -v patchelf >/dev/null 2>&1; then
  for lib in "${OUT}"/libggml-*.so "${OUT}/lib"/libsd-cuda.so; do
    [[ -f "${lib}" ]] || continue
    patchelf --set-rpath '$ORIGIN/lib' "${lib}" || true
  done
fi
if [[ "$(uname -s)" != Darwin ]] && command -v readelf >/dev/null 2>&1; then
  if [[ -f "${OUT}/lib/libstable-diffusion.so" ]] && \
     readelf -d "${OUT}/lib/libstable-diffusion.so" | grep -q 'libcuda\.so'; then
    echo "libstable-diffusion.so still requires libcuda; AMD and Intel machines cannot start it" >&2
    exit 1
  fi
fi

echo "==> ${OUT}/scalattice-sd"
echo "    libs: ${OUT}/lib"
