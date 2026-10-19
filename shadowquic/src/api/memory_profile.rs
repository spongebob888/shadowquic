use super::{ApiError, HttpResponse, StatusCode};

#[cfg(any(
    target_os = "android",
    all(
        any(target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod supported {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, sync::Mutex};

    use super::*;

    static DUMP_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn dump() -> Result<Vec<u8>, ApiError> {
        let _guard = DUMP_LOCK.try_lock().map_err(|_| {
            ApiError::new(
                StatusCode::CONFLICT,
                "memory_profile_in_progress",
                "a heap profile dump is already in progress",
            )
        })?;
        let mut enabled = false;
        let mut size = size_of::<bool>();
        // SAFETY: opt.prof returns a bool into a correctly sized live variable.
        let result = unsafe {
            tikv_jemalloc_sys::mallctl(
                c"opt.prof".as_ptr(),
                (&mut enabled as *mut bool).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || !enabled {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "memory_profiling_disabled",
                "jemalloc profiling is disabled; restart with _RJEM_MALLOC_CONF=prof:true,prof_active:true",
            ));
        }
        let file = tempfile::NamedTempFile::new().map_err(failed)?;
        let path = CString::new(file.path().as_os_str().as_bytes()).map_err(failed)?;
        let mut path_ptr = path.as_ptr();
        // SAFETY: prof.dump takes a pointer to a C string pointer. Both the
        // pointer and its NUL-terminated path live through this synchronous call.
        let result = unsafe {
            tikv_jemalloc_sys::mallctl(
                c"prof.dump".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&mut path_ptr as *mut *const std::ffi::c_char).cast(),
                size_of::<*const std::ffi::c_char>(),
            )
        };
        if result != 0 {
            return Err(failed(std::io::Error::from_raw_os_error(result)));
        }
        std::fs::read(file.path()).map_err(failed)
    }

    fn failed(error: impl std::fmt::Display) -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_profile_failed",
            format!("failed to dump heap profile: {error}"),
        )
    }
}

pub(super) async fn dump() -> Result<HttpResponse, ApiError> {
    #[cfg(any(
        target_os = "android",
        all(
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    {
        let data = tokio::task::spawn_blocking(supported::dump)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "memory_profile_failed",
                    error,
                )
            })??;
        return Ok(hyper::Response::builder()
            .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
            .header(
                hyper::header::CONTENT_DISPOSITION,
                "attachment; filename=shadowquic.heap",
            )
            .header(hyper::header::CACHE_CONTROL, "no-store")
            .body(http_body_util::Full::new(bytes::Bytes::from(data)))
            .unwrap());
    }
    #[cfg(not(any(
        target_os = "android",
        all(
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "memory_profiling_unavailable",
        "jemalloc profiling is unavailable on this target",
    ))
}
