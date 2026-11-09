// Thin CLI over stable-diffusion.cpp. The agent spawns this on the slot the
// server assigned. GPU choice is the process environment (CUDA_VISIBLE_DEVICES
// / GGML_VK_VISIBLE_DEVICES), the same variables the chat worker sets.
//
// Build with scripts/build-stable-diffusion.sh. Do not link this into the
// agent binary: its ggml must stay in its own lib directory so it does not
// collide with llama.cpp's ggml.

#include "stable-diffusion.h"

#include <cstdint>
#include <cstring>
#include <fstream>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

namespace {

std::string flag(int argc, char** argv, const char* name) {
    const std::string key = name;
    for (int i = 1; i + 1 < argc; ++i) {
        if (key == argv[i]) {
            return argv[i + 1];
        }
    }
    return {};
}

bool has_flag(int argc, char** argv, const char* name) {
    const std::string key = name;
    for (int i = 1; i < argc; ++i) {
        if (key == argv[i]) {
            return true;
        }
    }
    return false;
}

int64_t flag_i64(int argc, char** argv, const char* name, int64_t fallback) {
    const std::string raw = flag(argc, argv, name);
    if (raw.empty()) {
        return fallback;
    }
    try {
        return std::stoll(raw);
    } catch (...) {
        return fallback;
    }
}

bool read_raw(const std::string& path, sd_image_t* image, std::vector<uint8_t>* storage) {
    std::ifstream in(path, std::ios::binary);
    if (!in) {
        std::cerr << "scalattice-sd: cannot read " << path << "\n";
        return false;
    }
    uint32_t w = 0, h = 0, c = 0;
    in.read(reinterpret_cast<char*>(&w), 4);
    in.read(reinterpret_cast<char*>(&h), 4);
    in.read(reinterpret_cast<char*>(&c), 4);
    if (!in || w == 0 || h == 0 || (c != 3 && c != 4)) {
        std::cerr << "scalattice-sd: bad init image " << path << "\n";
        return false;
    }
    const size_t nbytes = static_cast<size_t>(w) * h * c;
    storage->resize(nbytes);
    in.read(reinterpret_cast<char*>(storage->data()), static_cast<std::streamsize>(nbytes));
    if (!in) {
        std::cerr << "scalattice-sd: short init image " << path << "\n";
        return false;
    }
    image->width = w;
    image->height = h;
    image->channel = c;
    image->data = storage->data();
    return true;
}

bool write_raw(const std::string& path, const sd_image_t& image) {
    if (image.data == nullptr || image.width == 0 || image.height == 0 || image.channel == 0) {
        std::cerr << "scalattice-sd: empty result image\n";
        return false;
    }
    std::ofstream out(path, std::ios::binary);
    if (!out) {
        std::cerr << "scalattice-sd: cannot write " << path << "\n";
        return false;
    }
    const uint32_t w = image.width;
    const uint32_t h = image.height;
    const uint32_t c = image.channel;
    out.write(reinterpret_cast<const char*>(&w), 4);
    out.write(reinterpret_cast<const char*>(&h), 4);
    out.write(reinterpret_cast<const char*>(&c), 4);
    const size_t nbytes = static_cast<size_t>(w) * h * c;
    out.write(reinterpret_cast<const char*>(image.data), static_cast<std::streamsize>(nbytes));
    return static_cast<bool>(out);
}

void on_progress(int step, int steps, float /*time*/, void* /*data*/) {
    std::cerr << "scalattice-sd: step " << step << "/" << steps << "\n";
}

const char* or_null(const std::string& value) {
    return value.empty() ? nullptr : value.c_str();
}

}  // namespace

int main(int argc, char** argv) {
    if (has_flag(argc, argv, "--help") || argc < 2) {
        std::cerr
            << "usage: scalattice-sd --output FILE [--model FILE] [--diffusion-model FILE]\n"
            << "         [--vae FILE] [--clip-l FILE] [--clip-g FILE] [--t5xxl FILE] [--llm FILE]\n"
            << "         --prompt TEXT [--negative TEXT] [--width N] [--height N] [--steps N]\n"
            << "         [--seed N] [--batch N] [--split-mode layer|row] [--init-rgb FILE]\n";
        return argc < 2 ? 2 : 0;
    }

    const std::string model = flag(argc, argv, "--model");
    const std::string diffusion = flag(argc, argv, "--diffusion-model");
    const std::string vae = flag(argc, argv, "--vae");
    const std::string clip_l = flag(argc, argv, "--clip-l");
    const std::string clip_g = flag(argc, argv, "--clip-g");
    const std::string t5 = flag(argc, argv, "--t5xxl");
    const std::string llm = flag(argc, argv, "--llm");
    const std::string prompt = flag(argc, argv, "--prompt");
    const std::string negative = flag(argc, argv, "--negative");
    const std::string output = flag(argc, argv, "--output");
    const std::string split = flag(argc, argv, "--split-mode");
    const std::string init_path = flag(argc, argv, "--init-rgb");
    const int64_t width = flag_i64(argc, argv, "--width", 0);
    const int64_t height = flag_i64(argc, argv, "--height", 0);
    const int64_t steps = flag_i64(argc, argv, "--steps", 0);
    const int64_t seed = flag_i64(argc, argv, "--seed", -1);
    const int64_t batch = flag_i64(argc, argv, "--batch", 1);

    if (output.empty() || prompt.empty()) {
        std::cerr << "scalattice-sd: --output and --prompt are required\n";
        return 2;
    }
    if (model.empty() && diffusion.empty()) {
        std::cerr << "scalattice-sd: --model or --diffusion-model is required\n";
        return 2;
    }

    sd_ctx_params_t ctx_params;
    sd_ctx_params_init(&ctx_params);
    ctx_params.model_path = or_null(model);
    ctx_params.diffusion_model_path = or_null(diffusion);
    ctx_params.vae_path = or_null(vae);
    ctx_params.clip_l_path = or_null(clip_l);
    ctx_params.clip_g_path = or_null(clip_g);
    ctx_params.t5xxl_path = or_null(t5);
    ctx_params.llm_path = or_null(llm);
    ctx_params.n_threads = static_cast<int>(std::thread::hardware_concurrency());
    if (ctx_params.n_threads < 1) {
        ctx_params.n_threads = 4;
    }
    ctx_params.split_mode = or_null(split);
    ctx_params.enable_mmap = true;

    sd_set_progress_callback(on_progress, nullptr);
    sd_ctx_t* ctx = new_sd_ctx(&ctx_params);
    if (ctx == nullptr) {
        std::cerr << "scalattice-sd: failed to load the image model\n";
        return 1;
    }

    sd_img_gen_params_t gen;
    sd_img_gen_params_init(&gen);
    gen.prompt = prompt.c_str();
    gen.negative_prompt = negative.empty() ? nullptr : negative.c_str();
    gen.width = static_cast<int>(width);
    gen.height = static_cast<int>(height);
    gen.seed = seed;
    gen.batch_count = static_cast<int>(batch < 1 ? 1 : batch);
    if (steps > 0) {
        gen.sample_params.sample_steps = static_cast<int>(steps);
    }

    std::vector<uint8_t> init_storage;
    if (!init_path.empty()) {
        if (!read_raw(init_path, &gen.init_image, &init_storage)) {
            free_sd_ctx(ctx);
            return 1;
        }
        gen.strength = 0.75f;
    }

    sd_image_generation_result_t* result = nullptr;
    const bool ok = generate_image(ctx, &gen, &result);
    int code = 0;
    if (!ok || result == nullptr || result->count < 1 || result->items == nullptr) {
        std::cerr << "scalattice-sd: generation failed\n";
        code = 1;
    } else {
        for (int i = 0; i < result->count; ++i) {
            const std::string path = output + "." + std::to_string(i);
            if (!write_raw(path, result->items[i].image)) {
                code = 1;
                break;
            }
        }
    }
    if (result != nullptr) {
        free_sd_image_generation_result(result);
    }
    free_sd_ctx(ctx);
    return code;
}
