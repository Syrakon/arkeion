//! E/S posicional portable sobre `std`, sin `unsafe` y mockeable para
//! inyección de fallos (R4).

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// Archivo de base de datos con lecturas/escrituras posicionales (`&self`:
/// sin seek compartido, apto para lectores concurrentes).
#[derive(Debug)]
pub struct DbFile {
    file: File,
}

impl DbFile {
    /// Crea el archivo en exclusiva (falla si ya existe).
    pub fn create_new(path: &Path) -> io::Result<DbFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(DbFile { file })
    }

    pub fn open_rw(path: &Path) -> io::Result<DbFile> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(DbFile { file })
    }

    /// Lock advisory exclusivo (un solo proceso escritor, R13). `false` si
    /// otro handle lo mantiene. Se libera al cerrar el archivo.
    pub fn try_lock_exclusive(&self) -> io::Result<bool> {
        match self.file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn byte_len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::FileExt;

    impl super::DbFile {
        pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            self.file.read_exact_at(buf, offset)
        }

        pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            self.file.write_all_at(buf, offset)
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::fs::FileExt;

    impl super::DbFile {
        pub fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.file.seek_read(buf, offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF en lectura posicional",
                        ));
                    }
                    Ok(n) => {
                        buf = &mut buf[n..];
                        offset += n as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        pub fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.file.seek_write(buf, offset) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "escritura posicional de 0 bytes",
                        ));
                    }
                    Ok(n) => {
                        buf = &buf[n..];
                        offset += n as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }
}

/// Persiste la entrada de directorio tras crear o renombrar el archivo (R4).
/// No-op fuera de Unix.
pub fn sync_parent_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.bin");
        let f = DbFile::create_new(&path).unwrap();

        f.write_all_at(b"hola", 100).unwrap();
        f.write_all_at(b"mundo", 0).unwrap();

        let mut buf = [0u8; 4];
        f.read_exact_at(&mut buf, 100).unwrap();
        assert_eq!(&buf, b"hola");
        assert_eq!(f.byte_len().unwrap(), 104);
        sync_parent_dir(&path).unwrap();
    }

    #[test]
    fn create_new_fails_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.bin");
        DbFile::create_new(&path).unwrap();
        assert!(DbFile::create_new(&path).is_err());
    }

    #[test]
    fn exclusive_lock_blocks_second_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.bin");
        let a = DbFile::create_new(&path).unwrap();
        assert!(a.try_lock_exclusive().unwrap());

        let b = DbFile::open_rw(&path).unwrap();
        assert!(!b.try_lock_exclusive().unwrap());

        drop(a);
        assert!(b.try_lock_exclusive().unwrap());
    }
}
