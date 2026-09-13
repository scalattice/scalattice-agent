#!/usr/bin/env python3
"""Diffusers image worker for scalattice-agent.

Loads whatever Hugging Face Diffusers repo the catalog row points at
(model_index.json). NVIDIA CUDA, AMD (ROCm on Linux, DirectML on Windows), Intel Arc (PyTorch XPU),
or Apple Silicon MPS. Isolated CPython + venv under ~/.cache/scalattice;
never uses the machine's PATH Python.

Setup runs when an image SKU is installed (`--setup`). Generate assumes that
venv already exists. Qwen-Image keeps native sizes/CFG; other checkpoints use
pipeline defaults. SCALATTICE_QWEN_IMAGE_STUB=1 returns a 1x1 PNG.
"""
from __future__ import annotations

import json
import os
import sys
import threading
import traceback
from pathlib import Path

# Tiny 1x1 PNG (valid) for stub / tests.
STUB_PNG_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)

POSITIVE_EN = ", Ultra HD, 4K, cinematic composition."
POSITIVE_ZH = ", 超清，4K，电影级构图."

STUB_ENV = ("1", "true", "TRUE", "yes")
HOST_PYTHON_KEYS = (
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONUSERBASE",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_PYTHON_EXE",
    "CONDA_SHLVL",
    "PIP_USER",
    "PIP_REQUIRE_VIRTUALENV",
    "UV_SYSTEM_PYTHON",
    "PYENV_VERSION",
    "PYENV_VIRTUAL_ENV",
)


def isolate_from_host_python() -> None:
    """Do not inherit conda/pyenv/user-site from the provider's Python."""
    for key in HOST_PYTHON_KEYS:
        os.environ.pop(key, None)
    os.environ["PYTHONNOUSERSITE"] = "1"
    os.environ["PIP_USER"] = "0"
    os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "1")
    os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")
    # hf_xet reconstructs shards via a background writer. That path surfaces
    # disk/CAS failures as "Background writer channel closed" and can re-run
    # at invoke even when the snapshot is already on disk. HTTP + local files.
    os.environ["HF_HUB_DISABLE_XET"] = "1"


isolate_from_host_python()


def emit(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def hf_auth(hf_token: str | None):
    """Public snapshots do not need a token. `False` is anonymous-on-purpose
    (huggingface_hub will not nag for HF_TOKEN). A real token is only for gated repos."""
    token = str(hf_token or "").strip()
    return token if token else False


def progress(phase: str, pct: float | None = None) -> None:
    row: dict = {"type": "progress", "phase": phase}
    if pct is not None:
        row["pct"] = pct
    emit(row)


def fail(code: str, detail: str) -> None:
    emit({"type": "error", "error": code, "detail": detail[:400]})
    sys.exit(1)


def explain_hf_err(err: BaseException) -> str:
    msg = str(err).strip() or err.__class__.__name__
    low = msg.lower()
    if "background writer" in low or "file reconstruction" in low:
        return (
            f"{msg}. Hugging Face Xet reconstruction failed "
            "(often a full disk or a half-written snapshot). "
            "Free disk space and reinstall the image model."
        )
    return msg


def hub_repo_cache_dir(cache_dir: str, repo: str) -> Path:
    key = "models--" + repo.strip().replace("/", "--")
    return Path(cache_dir) / key


def local_snapshot_dir(cache_dir: str, repo: str, revision: str, hint: str = "") -> str:
    if hint:
        path = Path(hint)
        if (path / "model_index.json").is_file():
            return str(path)
    if not cache_dir or not repo:
        return ""
    root = hub_repo_cache_dir(cache_dir, repo)
    snapshots = root / "snapshots"
    rev = (revision or "main").strip() or "main"
    ref = root / "refs" / rev
    try:
        if ref.is_file():
            hashed = ref.read_text(encoding="utf-8").strip()
            snap = snapshots / hashed
            if (snap / "model_index.json").is_file():
                return str(snap)
    except OSError:
        pass
    try:
        if snapshots.is_dir():
            for child in sorted(snapshots.iterdir()):
                if child.is_dir() and (child / "model_index.json").is_file():
                    return str(child)
    except OSError:
        pass
    return ""


def looks_cjk(text: str) -> bool:
    for ch in text:
        if "\u4e00" <= ch <= "\u9fff":
            return True
    return False


def stub_enabled() -> bool:
    return os.environ.get("SCALATTICE_QWEN_IMAGE_STUB", "").strip() in STUB_ENV


def start_heartbeat(phase_holder: list[str]) -> threading.Event:
    """Keep the agent/router stall timers alive through pip, HF download, and denoise."""
    stop = threading.Event()

    def beat() -> None:
        while not stop.wait(12):
            progress(phase_holder[0] or "working")

    threading.Thread(target=beat, daemon=True).start()
    return stop


def pip_env() -> dict:
    env = os.environ.copy()
    for key in HOST_PYTHON_KEYS:
        env.pop(key, None)
    env["PYTHONNOUSERSITE"] = "1"
    env["PIP_USER"] = "0"
    return env


def ensure_venv_and_deps(venv_dir: Path, torch_index: str, extra_pip: list[str]) -> Path:
    import subprocess
    import venv

    venv_python = venv_dir / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    if not venv_python.is_file():
        progress("venv", 5)
        venv_dir.mkdir(parents=True, exist_ok=True)
        venv.create(str(venv_dir), with_pip=True)
    if not venv_python.is_file():
        fail("image_runtime_missing", "Could not create a Python venv for Diffusers.")

    marker = venv_dir / ".deps_ok_v3"
    if marker.is_file():
        return venv_python

    progress("install", 10)
    env = pip_env()
    pip_base = [str(venv_python), "-m", "pip", "install", "--upgrade", "pip"]
    subprocess.check_call(pip_base, stdout=subprocess.DEVNULL, env=env)
    progress("install", 20)
    torch_cmd = [str(venv_python), "-m", "pip", "install", "torch"]
    if torch_index.strip():
        torch_cmd.extend(["--index-url", torch_index.strip()])
    subprocess.check_call(torch_cmd, stdout=subprocess.DEVNULL, env=env)
    progress("install", 45)
    pkgs = [
        str(venv_python),
        "-m",
        "pip",
        "install",
        "diffusers>=0.35.0",
        "transformers",
        "accelerate",
        "safetensors",
        "pillow",
        "huggingface_hub",
        "sentencepiece",
        "protobuf",
        "peft",
        "einops",
    ]
    subprocess.check_call(pkgs, stdout=subprocess.DEVNULL, env=env)
    extras = [str(p).strip() for p in extra_pip if str(p).strip()]
    if extras:
        progress("install", 50)
        subprocess.check_call(
            [str(venv_python), "-m", "pip", "install", *extras],
            stdout=subprocess.DEVNULL,
            env=env,
        )
    marker.write_text("ok\n", encoding="utf-8")
    progress("install", 55)
    return venv_python


def prefetch_repo(job: dict) -> None:
    repo = str(job.get("repo") or "").strip()
    if not repo:
        return
    revision = str(job.get("revision") or "main").strip() or "main"
    cache_dir = str(job.get("cache_dir") or "").strip()
    hf_token = str(job.get("hf_token") or "").strip() or None
    if cache_dir:
        os.environ["HF_HUB_CACHE"] = cache_dir
        os.environ["HUGGINGFACE_HUB_CACHE"] = cache_dir
    progress("download", 60)
    try:
        from huggingface_hub import snapshot_download
    except Exception as err:
        fail("image_runtime_missing", f"huggingface_hub import failed: {err}")
    kwargs = {"repo_id": repo, "revision": revision, "token": hf_auth(hf_token)}
    if cache_dir:
        kwargs["cache_dir"] = cache_dir
    try:
        snapshot_download(**kwargs)
    except Exception as err:
        fail(
            "model_load_failed",
            f"Diffusers snapshot download failed for {repo}: {explain_hf_err(err)}",
        )
    progress("download", 100)


def stub_result(n: int) -> None:
    images = [{"mime": "image/png", "data": STUB_PNG_B64} for _ in range(max(1, n))]
    emit({"type": "result", "images": images})


def decode_input_images(job: dict) -> list:
    raw_list = job.get("images") or job.get("input_images") or []
    if not isinstance(raw_list, list):
        raw_list = [raw_list]
    out = []
    import base64
    from io import BytesIO

    try:
        from PIL import Image
    except Exception as err:
        fail("image_runtime_missing", f"Pillow import failed: {err}")
        return []

    for item in raw_list[:4]:
        if not isinstance(item, dict):
            continue
        payload = str(item.get("data") or "").strip()
        if not payload:
            continue
        if payload.startswith("data:"):
            _, _, payload = payload.partition(",")
        try:
            blob = base64.b64decode(payload)
        except Exception:
            fail("invalid_image", "Could not decode a reference image.")
        if not blob:
            continue
        try:
            img = Image.open(BytesIO(blob)).convert("RGB")
        except Exception:
            fail("invalid_image", "A reference image could not be decoded.")
        out.append(img)
    return out


def pipeline_params(pipe) -> set[str]:
    import inspect

    fn = getattr(type(pipe), "__call__", None) or getattr(pipe, "__call__", None)
    try:
        return set(inspect.signature(fn).parameters)
    except (TypeError, ValueError):
        return set()


def round_dim(n: int) -> int:
    n = max(64, min(2048, int(n)))
    return max(64, n - (n % 16))


def repo_slug(repo: str) -> str:
    return repo.strip().lower().replace("_", "-")


def is_qwen_image_family(repo: str) -> bool:
    return "qwen-image" in repo_slug(repo)


def is_qwen_image_t2i(repo: str) -> bool:
    slug = repo_slug(repo)
    return "qwen-image" in slug and "edit" not in slug


def maybe_enable_vram_helpers(pipe) -> None:
    for name in ("enable_vae_slicing", "enable_attention_slicing"):
        fn = getattr(pipe, name, None)
        if callable(fn):
            try:
                fn()
            except Exception:
                pass


def pick_torch_device(want: str):
    import torch

    want = (want or "").strip().lower()
    if want == "mps":
        mps_ok = hasattr(torch.backends, "mps") and torch.backends.mps.is_available()
        if not mps_ok:
            fail(
                "image_accelerator_required",
                "Apple Silicon MPS is not available on this Mac.",
            )
        return "mps", torch.float16
    if want == "xpu":
        xpu_ok = hasattr(torch, "xpu") and torch.xpu.is_available()
        if not xpu_ok:
            fail(
                "image_accelerator_required",
                "Intel Arc XPU is not available. Install a current Intel GPU driver (Level Zero / compute runtime).",
            )
        bf16 = getattr(torch.xpu, "is_bf16_supported", lambda: False)()
        return "xpu", torch.bfloat16 if bf16 else torch.float16
    if want == "dml":
        try:
            import torch_directml

            return torch_directml.device(), torch.float16
        except Exception as err:
            fail(
                "image_accelerator_required",
                f"DirectML is not available on this Windows GPU ({err}).",
            )
    if torch.cuda.is_available():
        dtype = torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16
        return "cuda", dtype
    if want == "rocm":
        fail(
            "image_accelerator_required",
            "Image generation needs a working AMD ROCm GPU (PyTorch HIP).",
        )
    fail(
        "image_accelerator_required",
        "Image generation needs an NVIDIA CUDA GPU, AMD GPU (ROCm/DirectML), Intel Arc (XPU), or Apple Silicon (MPS).",
    )
    return "cpu", None


def generate(job: dict, phase_holder: list[str]) -> None:
    prompt = str(job.get("prompt") or "").strip()
    if not prompt:
        fail("image_prompt_required", "Empty prompt")
    n = int(job.get("n") or 1)
    n = max(1, min(4, n))
    width = int(job.get("width") or 0)
    height = int(job.get("height") or 0)
    seed = job.get("seed")
    repo = str(job.get("repo") or "").strip()
    if not repo:
        fail("model_load_failed", "Catalog image models need a Hugging Face Diffusers repo.")
    revision = str(job.get("revision") or "main").strip() or "main"
    cache_dir = str(job.get("cache_dir") or "").strip()
    snapshot_hint = str(job.get("snapshot_dir") or "").strip()
    hf_token = str(job.get("hf_token") or "").strip() or None
    qwen = is_qwen_image_family(repo)
    want_device = str(job.get("device") or "").strip().lower()

    if stub_enabled():
        progress("generate", 90)
        stub_result(n)
        return

    phase_holder[0] = "load"
    try:
        import torch
        from diffusers import DiffusionPipeline
    except Exception as err:
        fail("image_runtime_missing", f"PyTorch/Diffusers import failed: {err}")

    input_images = decode_input_images(job)
    device, dtype = pick_torch_device(want_device)

    if cache_dir:
        os.environ["HF_HUB_CACHE"] = cache_dir
        os.environ["HUGGINGFACE_HUB_CACHE"] = cache_dir

    source = local_snapshot_dir(cache_dir, repo, revision, snapshot_hint)
    if not source:
        fail(
            "model_load_failed",
            f"Diffusers snapshot is not on disk for {repo}. Reinstall the image model.",
        )

    progress("load", 40)
    kwargs = {
        "local_files_only": True,
        "token": hf_auth(hf_token),
    }
    try:
        try:
            pipe = DiffusionPipeline.from_pretrained(source, dtype=dtype, **kwargs)
        except TypeError:
            pipe = DiffusionPipeline.from_pretrained(source, torch_dtype=dtype, **kwargs)
        pipe = pipe.to(device)
        maybe_enable_vram_helpers(pipe)
        if hasattr(pipe, "set_progress_bar_config"):
            pipe.set_progress_bar_config(disable=True)
    except Exception as err:
        fail(
            "model_load_failed",
            f"Diffusers load failed for {repo}: {explain_hf_err(err)}",
        )

    params = pipeline_params(pipe)
    if input_images and "image" not in params and "images" not in params:
        fail(
            "image_edit_unsupported",
            "This Diffusers checkpoint is text-to-image only. Point the catalog at an edit pipeline that accepts image or images.",
        )

    if input_images and (width < 64 or height < 64):
        width = round_dim(input_images[0].size[0])
        height = round_dim(input_images[0].size[1])
    if width < 64:
        width = 1328 if qwen else 1024
    if height < 64:
        height = 1328 if qwen else 1024

    phase_holder[0] = "generate"
    progress("load", 70)
    if is_qwen_image_t2i(repo) and not input_images:
        magic = POSITIVE_ZH if looks_cjk(prompt) else POSITIVE_EN
        if magic.strip(" ,") not in prompt:
            prompt = prompt.rstrip() + magic

    generator = None
    if seed is not None:
        try:
            generator = torch.Generator(device=device).manual_seed(int(seed) & 0xFFFFFFFF)
        except Exception:
            try:
                generator = torch.Generator().manual_seed(int(seed) & 0xFFFFFFFF)
            except Exception:
                generator = None

    images_out = []
    for i in range(n):
        progress("generate", 75 + (20 * i / n))
        call = {
            "prompt": prompt,
            "width": width,
            "height": height,
            "generator": generator,
        }
        if qwen:
            call["negative_prompt"] = " "
            call["num_inference_steps"] = 50
            call["true_cfg_scale"] = 4.0
        if input_images:
            image_arg = input_images if len(input_images) > 1 else input_images[0]
            if "image" in params:
                call["image"] = image_arg
            elif "images" in params:
                call["images"] = input_images
        filtered = {k: v for k, v in call.items() if k in params or not params}
        try:
            result = pipe(**filtered)
            image = result.images[0]
        except torch.cuda.OutOfMemoryError:
            fail("insufficient_vram", f"{repo} ran out of GPU memory.")
        except Exception as err:
            oom = getattr(torch, "xpu", None)
            xpu_oom = getattr(oom, "OutOfMemoryError", ()) if oom is not None else ()
            if xpu_oom and isinstance(err, xpu_oom):
                fail("insufficient_vram", f"{repo} ran out of GPU memory.")
            if "out of memory" in str(err).lower():
                fail("insufficient_vram", f"{repo} ran out of GPU memory.")
            if isinstance(err, TypeError) and input_images:
                fail(
                    "image_edit_unsupported",
                    f"This checkpoint rejected reference images: {err}",
                )
            if isinstance(err, TypeError):
                fail("inference_failed", f"Image generate failed: {err}")
            fail("inference_failed", f"Image generate failed: {err}")
        import base64
        from io import BytesIO

        buf = BytesIO()
        image.save(buf, format="PNG")
        images_out.append(
            {
                "mime": "image/png",
                "data": base64.b64encode(buf.getvalue()).decode("ascii"),
            }
        )
        if generator is not None:
            try:
                generator = torch.Generator(device=device).manual_seed(
                    ((int(seed) & 0xFFFFFFFF) + i + 1) & 0xFFFFFFFF
                )
            except Exception:
                generator = torch.Generator().manual_seed(
                    ((int(seed) & 0xFFFFFFFF) + i + 1) & 0xFFFFFFFF
                )

    progress("generate", 100)
    emit({"type": "result", "images": images_out})


def parse_argv() -> tuple[bool, Path | None]:
    args = [a for a in sys.argv[1:] if a]
    setup = False
    if args and args[0] == "--setup":
        setup = True
        args = args[1:]
    if not args:
        return setup, None
    return setup, Path(args[0])


def load_job(job_path: Path | None) -> dict:
    if job_path is not None:
        raw = job_path.read_text(encoding="utf-8")
    else:
        raw = sys.stdin.read()
    if not raw.strip():
        fail("invalid_request", "Empty worker job")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as err:
        fail("invalid_request", f"Invalid JSON: {err}")
    return {}


def maybe_unlink_job_file(job_path: Path | None) -> None:
    if job_path is None:
        return
    try:
        job_path.unlink(missing_ok=True)
    except TypeError:
        try:
            job_path.unlink()
        except OSError:
            pass
    except OSError:
        pass


def extra_pip_from_job(job: dict) -> list[str]:
    raw = job.get("extra_pip") or []
    if isinstance(raw, str):
        raw = [raw]
    if not isinstance(raw, list):
        return []
    return [str(x).strip() for x in raw if str(x).strip()]


def reenter_venv(venv_python: Path, setup: bool, job_path: Path | None) -> None:
    if Path(sys.executable).resolve() == venv_python.resolve():
        return
    argv = [str(venv_python), os.path.abspath(__file__)]
    if setup:
        argv.append("--setup")
    if job_path is not None:
        argv.append(str(job_path))
    os.execv(str(venv_python), argv)


def main() -> None:
    setup, job_path = parse_argv()
    job = load_job(job_path)
    setup = setup or str(job.get("mode") or "").strip().lower() == "setup"

    if stub_enabled():
        if setup:
            progress("install", 100)
            emit({"type": "result", "images": []})
            maybe_unlink_job_file(job_path)
            return
        n = max(1, min(4, int(job.get("n") or 1)))
        progress("generate", 90)
        stub_result(n)
        maybe_unlink_job_file(job_path)
        return

    venv_dir = Path(str(job.get("venv_dir") or "")).expanduser()
    if not str(venv_dir):
        fail("image_runtime_missing", "Missing venv_dir")

    phase_holder = ["venv"]
    stop = start_heartbeat(phase_holder)
    try:
        venv_python = ensure_venv_and_deps(
            venv_dir,
            str(job.get("torch_index") or ""),
            extra_pip_from_job(job),
        )
        reenter_venv(venv_python, setup, job_path)
        if setup:
            phase_holder[0] = "download"
            prefetch_repo(job)
            emit({"type": "result", "images": []})
            maybe_unlink_job_file(job_path)
            return
        maybe_unlink_job_file(job_path)
        phase_holder[0] = "download"
        generate(job, phase_holder)
    except SystemExit:
        raise
    except Exception as err:
        if setup:
            fail("image_runtime_missing", f"venv/deps failed: {err}")
        fail("inference_failed", traceback.format_exc()[-400:])
    finally:
        stop.set()


if __name__ == "__main__":
    main()
