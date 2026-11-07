use socket2::Socket;
use std::io;

pub fn bind_device(_socket: &Socket, _device_name: &str) -> io::Result<()> {
    tracing::warn!(
        event = "connectivity:interface_binding_unsupported",
        "bind interface not supported on this platform"
    );
    Ok(())
}
