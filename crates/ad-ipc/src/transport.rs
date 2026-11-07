//! UDS 传输辅助：socket 建立（含存活检查）与连接。

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// 绑定控制 socket。
///
/// 若路径上已有 socket：先尝试连接——连得上说明有存活实例，报错；
/// 否则视为陈旧残留，删除后重建。权限 0600，父目录 0700。
pub fn bind_control_socket(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("socket {} 已被存活实例占用", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// 连接控制 socket。
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}
