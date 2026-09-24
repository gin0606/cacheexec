use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::Path,
};

pub fn key(argv: &[OsString], cwd: &Path, extra: Option<&OsStr>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cacheexec-key-v1");
    let mut field = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    field(cwd.as_os_str().as_bytes());
    field(&(argv.len() as u64).to_le_bytes());
    for arg in argv {
        field(arg.as_bytes());
    }
    field(&[u8::from(extra.is_some())]);
    if let Some(extra) = extra {
        field(extra.as_bytes());
    }
    format!("{:x}", digest.finalize())
}
