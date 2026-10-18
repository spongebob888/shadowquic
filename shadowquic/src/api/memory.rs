use super::{ApiError, StatusCode, Value};

#[cfg(any(
    target_os = "android",
    all(
        any(target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod supported {
    use std::ffi::CStr;

    pub(super) fn stats() -> Option<[usize; 3]> {
        let mut epoch = 1u64;
        // SAFETY: epoch has the exact type, size, and lifetime mallctl requires.
        let result = unsafe {
            tikv_jemalloc_sys::mallctl(
                c"epoch".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&mut epoch as *mut u64).cast(),
                size_of::<u64>(),
            )
        };
        if result != 0 {
            return None;
        }
        Some([
            read(c"stats.allocated")?,
            read(c"stats.active")?,
            read(c"stats.resident")?,
        ])
    }

    fn read(name: &CStr) -> Option<usize> {
        let mut value = 0usize;
        let mut size = size_of::<usize>();
        // SAFETY: These stats are size_t values. Both output pointers refer to
        // correctly sized live variables throughout the synchronous call.
        let result = unsafe {
            tikv_jemalloc_sys::mallctl(
                name.as_ptr(),
                (&mut value as *mut usize).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (result == 0 && size == size_of::<usize>()).then_some(value)
    }
}

pub(super) fn get() -> Result<Value, ApiError> {
    #[cfg(any(
        target_os = "android",
        all(
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    if let Some([allocated, active, resident]) = supported::stats() {
        return Ok(serde_json::json!({
            "allocator": "jemalloc", "allocated_bytes": allocated,
            "active_bytes": active, "resident_bytes": resident,
        }));
    }
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "memory_stats_unavailable",
        "jemalloc statistics are unavailable on this target",
    ))
}
