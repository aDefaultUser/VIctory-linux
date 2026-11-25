//! `d3d12_xs.dll`: virtual memory setup, device creation, root signature
//! serialization. The device and everything it creates live in
//! `victory-gpu`.

use std::ffi::c_void;

use victory_gpu::device;
use victory_gpu::object::{HRESULT, Iid, Object};

use crate::{HleExport, HleModule};

/// Device creation; first hands the device the title's general allocator
/// (`XMemAlloc`, which honours the title's hooks) for resource memory.
extern "system" fn create_device(
    adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut *const Object,
) -> HRESULT {
    victory_gpu::resource::use_allocator(super::xmem::XMemAlloc, super::xmem::XMemFree);
    victory_gpu::resource::use_page_mapper(super::xmem::map_alias);
    #[cfg(windows)]
    victory_gpu::on_failure(host_gpu_failed);
    device::create(adapter, parameters, iid, out)
}

/// A host GPU failure ends the run like a crash: reported to the crash log,
/// then the process ends as the console ends a title.
#[cfg(windows)]
fn host_gpu_failed(what: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    victory_loader::log_crash(&format!("VIctory: host GPU failure: {what}"));
    const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A_0005;
    // SAFETY: ends our own process.
    unsafe { TerminateProcess(GetCurrentProcess(), DXGI_ERROR_DEVICE_REMOVED) };
    unreachable!("the process has ended")
}

/// The offline warm-up: see `victory_gpu::shader_cache::warm`.
pub fn warm_shader(container: &[u8]) -> Result<bool, String> {
    #[cfg(windows)]
    {
        victory_gpu::shader_cache::warm(container)
    }
    #[cfg(not(windows))]
    {
        let _ = container;
        Err("no shader cache on this host".into())
    }
}

/// The warm-up's host: see `victory_gpu::pipeline::use_host_device`.
pub fn warm_host() -> bool {
    #[cfg(windows)]
    {
        victory_gpu::pipeline::use_host_device()
    }
    #[cfg(not(windows))]
    false
}

/// The warm-up's compute pipelines: see `victory_gpu::pipeline::warm_compute`.
/// `gate` is handed the pipeline's name in the pipeline library just
/// before the host is asked for it, and decides whether it is.
pub fn warm_compute(container: &[u8], gate: Box<dyn FnMut(&str) -> bool>) -> Option<bool> {
    #[cfg(windows)]
    {
        victory_gpu::pipeline::warm_compute(container, gate)
    }
    #[cfg(not(windows))]
    {
        let _ = (container, gate);
        None
    }
}

/// See `victory_gpu::pipeline::host_driver_id`.
pub fn host_driver_id() -> Option<String> {
    #[cfg(windows)]
    {
        victory_gpu::pipeline::host_driver_id()
    }
    #[cfg(not(windows))]
    None
}

/// Writes the pipeline library now if pipelines were added.
pub fn save_pipeline_cache() {
    #[cfg(windows)]
    victory_gpu::pipeline::save_pipeline_cache();
}

/// The folder the title's shader containers are kept in, beside the
/// shader cache folder `shaders` (`VICTORY_SHADER_SOURCES=1`).
pub fn shader_sources_dir(shaders: &std::path::Path) -> std::path::PathBuf {
    shaders.with_file_name("shader-sources")
}

/// Keeps the host's translations of the title's shaders in `dir` (a
/// folder of the cache) across runs, and its compiled pipelines beside it
/// (`pipelines`). With `VICTORY_SHADER_SOURCES=1` (the Linux launcher's
/// warm-up sets it) the containers translated are kept too
/// (`shader-sources`), for `victory warm` before a later launch.
pub fn use_shader_cache(dir: std::path::PathBuf) {
    #[cfg(windows)]
    {
        if std::env::var_os("VICTORY_SHADER_SOURCES").is_some_and(|v| v == "1") {
            victory_gpu::shader_sources::keep_sources(shader_sources_dir(&dir));
        }
        victory_gpu::pipeline::use_pipeline_cache(dir.with_file_name("pipelines"));
        victory_gpu::shader_cache::use_shader_cache(dir);
    }
    #[cfg(not(windows))]
    let _ = dir;
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "d3d12x",
        dlls: &["d3d12_xs.dll"],
        exports: vec![
            HleExport {
                name: "D3D12ConfigureVirtualMemoryX",
                address: device::configure_virtual_memory as *const () as usize,
            },
            HleExport {
                name: "D3D12XboxCreateDevice",
                address: create_device as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeRootSignature",
                address: victory_gpu::root::serialize as *const () as usize,
            },
            HleExport {
                name: "D3D12SerializeVersionedRootSignature",
                address: victory_gpu::root::serialize_versioned as *const () as usize,
            },
            // The AMD upscaler's (Series S).
            HleExport {
                name: "D3D12CreateRootSignatureDeserializer",
                address: victory_gpu::root::create_deserializer as *const () as usize,
            },
            // A data import: the title reads the id at this address.
            HleExport {
                name: "IID_ID3D12Device",
                address: (&raw const device::IID) as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HleResolver, HostPolicy};
    use victory_loader::ImportResolver;
    use victory_loader::Resolution;
    use victory_pe::ImportSymbol;

    #[test]
    fn the_interface_id_import_is_bound_to_data() {
        let symbol = ImportSymbol::Name {
            hint: 0,
            name: "IID_ID3D12Device".into(),
        };
        let r = HleResolver::new(HostPolicy::none());
        let Resolution::Found { address, .. } = r.resolve("d3d12_xs.dll", &symbol) else {
            panic!("not bound");
        };
        // SAFETY: the address is our static id.
        assert_eq!(unsafe { *(address as *const [u8; 16]) }, device::IID);
    }
}
