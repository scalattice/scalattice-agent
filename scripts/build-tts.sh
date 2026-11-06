#!/usr/bin/env bash
# Speech runner, installed under native/tts/out. The agent starts it on the
# slot the server assigned:
#   *.gguf  -> llama-tts (llama.cpp tools/tts: Qwen3-TTS, Pocket, Soprano,
#              LFM3-Audio, Chatterbox, Drama). Built from its own llama.cpp
#              checkout so the chat pin (llama-cpp-2 0.1.154) stays put.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${ROOT}/native/tts/out"
JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu)"
mkdir -p "${OUT}/lib"

if [[ -z "${CUDA_PATH:-}" ]]; then
  if [[ -d /usr/local/cuda ]]; then
    export CUDA_PATH=/usr/local/cuda
  elif [[ -d /usr/local/cuda-12.6 ]]; then
    export CUDA_PATH=/usr/local/cuda-12.6
  fi
fi
if [[ -n "${CUDA_PATH:-}" && -d "${CUDA_PATH}/bin" ]]; then
  export PATH="${CUDA_PATH}/bin:${PATH}"
fi

build_llama_tts() {
  local src="${ROOT}/native/tts/llama.cpp"
  if [[ ! -d "${src}/.git" ]]; then
    git clone --depth 1 https://github.com/ggml-org/llama.cpp "${src}"
  fi
  # Fresh clones do not have --stream. The checkout under native/ is gitignored.
  local stream_patch="${ROOT}/scripts/patches/llama-tts-stream.patch"
  if [[ -f "${stream_patch}" ]] && git -C "${src}" apply --check "${stream_patch}" 2>/dev/null; then
    git -C "${src}" apply "${stream_patch}"
  fi
  local args=(
    -S "${src}"
    -B "${src}/build"
    -DCMAKE_BUILD_TYPE=Release
    -DGGML_NATIVE=OFF
    -DGGML_BACKEND_DL=ON
    -DLLAMA_BUILD_EXAMPLES=ON
    -DLLAMA_BUILD_TOOLS=ON
  )
  case "$(uname -s)" in
    Darwin)
      args+=(-DGGML_METAL=ON -DGGML_CUDA=OFF -DGGML_VULKAN=OFF)
      ;;
    *)
      args+=(-DGGML_CUDA=ON -DGGML_VULKAN=ON -DGGML_METAL=OFF)
      ;;
  esac
  cmake "${args[@]}"
  cmake --build "${src}/build" -j"${JOBS}" --target llama-tts
  local bin
  bin="$(find "${src}/build" -type f -name 'llama-tts' -o -name 'llama-tts.exe' | head -1)"
  if [[ -z "${bin}" ]]; then
    echo "llama-tts was not produced" >&2
    exit 1
  fi
  cp -a "${bin}" "${OUT}/llama-tts"
  chmod +x "${OUT}/llama-tts" || true
  local lib_src
  lib_src="$(dirname "${bin}")"
  mkdir -p "${OUT}/lib"
  shopt -s nullglob
  for lib in "${lib_src}"/libllama*.* "${lib_src}"/libggml*.* "${lib_src}"/libmtmd.* \
             "${src}/build/bin"/libllama*.* "${src}/build/bin"/libggml*.* "${src}/build/bin"/libmtmd.*; do
    cp -a "${lib}" "${OUT}/lib/" || true
  done
  if [[ -d "${src}/build/bin/backends" ]]; then
    mkdir -p "${OUT}/lib/backends"
    cp -a "${src}/build/bin/backends/." "${OUT}/lib/backends/" || true
  fi
  shopt -u nullglob
  # The cmake RUNPATH points at the build tree. The packaged binary has to
  # find these libraries beside itself.
  # ggml loads libggml-cuda.so / libggml-vulkan.so from the executable
  # directory, and only the one for a GPU that is actually present.
  for backend in libggml-cuda.so libggml-vulkan.so libggml-metal.so libggml-cpu.so; do
    if [[ -f "${OUT}/lib/${backend}" ]]; then
      mv "${OUT}/lib/${backend}" "${OUT}/${backend}"
    fi
  done
  if command -v patchelf >/dev/null 2>&1; then
    patchelf --set-rpath '$ORIGIN/lib' "${OUT}/llama-tts"
    for lib in "${OUT}"/libggml-*.so; do
      [[ -f "${lib}" ]] || continue
      patchelf --set-rpath '$ORIGIN/lib' "${lib}" || true
    done
    for lib in "${OUT}/lib"/lib*.so*; do
      [[ -L "${lib}" ]] && continue
      patchelf --set-rpath '$ORIGIN' "${lib}" || true
    done
  fi
}

rm -f "${OUT}/sherpa-onnx-offline-tts" "${OUT}/sherpa-onnx-offline-tts.exe"
build_llama_tts
echo "==> ${OUT}"
ls -l "${OUT}"
