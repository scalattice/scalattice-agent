//! Resolve llama.cpp ggml backend devices for Vulkan placement.
//!
//! Unit tests inject a fake device list so we can verify matching without AMD hardware.

use llama_cpp_2::{list_llama_ggml_backend_devices, LlamaBackendDevice, LlamaBackendDeviceType};
use std::sync::Mutex;

static TEST_OVERRIDE: Mutex<Option<Vec<LlamaBackendDevice>>> = Mutex::new(None);

/// Indices suitable for [`LlamaModelParams::with_devices`] on a Vulkan-only pool.
pub fn vulkan_ggml_device_indices() -> Vec<usize> {
    ggml_devices()
        .into_iter()
        .filter(is_vulkan_gpu_device)
        .map(|d| d.index)
        .collect()
}

/// Indices suitable for [`LlamaModelParams::with_devices`] on a Metal pool.
pub fn metal_ggml_device_indices() -> Vec<usize> {
    ggml_devices()
        .into_iter()
        .filter(is_metal_gpu_device)
        .map(|d| d.index)
        .collect()
}

fn is_metal_gpu_device(d: &LlamaBackendDevice) -> bool {
    if !d.backend.eq_ignore_ascii_case("metal") {
        return false;
    }
    matches!(
        d.device_type,
        LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
    )
}

fn is_vulkan_gpu_device(d: &LlamaBackendDevice) -> bool {
    if !d.backend.eq_ignore_ascii_case("vulkan") {
        return false;
    }
    matches!(
        d.device_type,
        LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
    )
}

fn ggml_devices() -> Vec<LlamaBackendDevice> {
    if let Ok(guard) = TEST_OVERRIDE.lock() {
        if let Some(ref devices) = *guard {
            return devices.clone();
        }
    }
    list_llama_ggml_backend_devices()
}

#[cfg(test)]
pub fn set_test_ggml_devices(devices: Option<Vec<LlamaBackendDevice>>) {
    *TEST_OVERRIDE.lock().unwrap() = devices;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vulkan_dev(index: usize, name: &str, igpu: bool) -> LlamaBackendDevice {
        LlamaBackendDevice {
            index,
            name: name.into(),
            description: name.into(),
            backend: "Vulkan".into(),
            memory_total: 8 << 30,
            memory_free: 7 << 30,
            device_type: if igpu {
                LlamaBackendDeviceType::IntegratedGpu
            } else {
                LlamaBackendDeviceType::Gpu
            },
        }
    }

    #[test]
    fn picks_vulkan_gpus_skips_cuda_and_cpu() {
        set_test_ggml_devices(Some(vec![
            LlamaBackendDevice {
                index: 0,
                name: "CUDA0".into(),
                description: "NVIDIA".into(),
                backend: "CUDA".into(),
                memory_total: 8 << 30,
                memory_free: 8 << 30,
                device_type: LlamaBackendDeviceType::Gpu,
            },
            vulkan_dev(1, "Vulkan0", false),
            vulkan_dev(2, "Vulkan1", true),
            LlamaBackendDevice {
                index: 3,
                name: "CPU".into(),
                description: "CPU".into(),
                backend: "CPU".into(),
                memory_total: 0,
                memory_free: 0,
                device_type: LlamaBackendDeviceType::Cpu,
            },
        ]));
        assert_eq!(vulkan_ggml_device_indices(), vec![1, 2]);
        set_test_ggml_devices(None);
    }
}
