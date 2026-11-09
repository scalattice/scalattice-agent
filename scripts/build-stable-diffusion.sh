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

mkdir -p "${ROOT}/native/sd"
if [[ ! -d "${SRC}/.git" ]]; then
  git clone --depth 1 --recursive https://github.com/leejet/stable-diffusion.cpp "${SRC}"
fi
# ggml is a submodule. A previous clone without --recursive leaves the directory empty.
if [[ ! -f "${SRC}/ggml/CMakeLists.txt" ]]; then
  git -C "${SRC}" submodule update --init --recursive --depth 1
fi
# CUDA kernels live in libsd-cuda.so and are opened only on NVIDIA machines.
patch_file="${ROOT}/scripts/patches/sd-load-cuda-on-demand.patch"
if [[ -f "${patch_file}" ]] && git -C "${SRC}" apply --check "${patch_file}" >/dev/null 2>&1; then
  git -C "${SRC}" apply "${patch_file}"
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
case "$(uname -s)" in
  Darwin)
    link_args+=(-Wl,-rpath,@loader_path/lib)
    ;;
  *)
    link_args+=(-Wl,-rpath-link,"${OUT}/lib" -Wl,-rpath,'$ORIGIN/lib')
    ;;
esac
"${cxx}" "${link_args[@]}" -o "${OUT}/scalattice-sd"
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
