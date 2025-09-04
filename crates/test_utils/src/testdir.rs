use std::{
    fs, io,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

pub struct TestDir {
    path: String,
    keep: bool,
}

impl TestDir {
    pub fn new(symlink: bool) -> TestDir {
        let temp_dir = std::env::temp_dir();
        // On MacOS builders on GitHub actions, the temp dir is a symlink, and
        // that causes problems down the line. Specifically:
        // * Cargo may emit different PackageId depending on the working directory
        // * rust-analyzer may fail to map LSP URIs to correct paths.
        //
        // Work-around this by canonicalizing. Note that we don't want to do this
        // on *every* OS, as on windows `canonicalize` itself creates problems.
        #[cfg(target_os = "macos")]
        let temp_dir = temp_dir.canonicalize().unwrap();

        let base = temp_dir.join("testdir");
        let pid = std::process::id();

        static CNT: AtomicUsize = AtomicUsize::new(0);
        for _ in 0..100 {
            let cnt = CNT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{pid}_{cnt}"));
            if path.is_dir() {
                continue;
            }
            fs::create_dir_all(&path).unwrap();

            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            if symlink {
                let symlink_path = base.join(format!("{pid}_{cnt}_symlink"));
                create_symlink_dir(path, &symlink_path).unwrap();

                return TestDir {
                    path: symlink_path.to_str().unwrap().to_string(),
                    keep: false,
                };
            }

            return TestDir {
                path: path.to_str().unwrap().to_string(),
                keep: false,
            };
        }
        panic!("Failed to create a temporary directory")
    }

    #[allow(unused)]
    pub(crate) fn keep(mut self) -> TestDir {
        self.keep = true;
        self
    }
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn path_for_uri(&self) -> String {
        // On Windows a path has slashes in it as well in an URI
        if cfg!(windows) {
            // Windows paths also start with a slash in a URI
            format!("/{}", self.path.replace('\\', "/"))
        } else {
            self.path.to_string()
        }
    }

    pub fn write_file(&self, rel_path: &str, text: &str) {
        let path = Path::new(&self.path).join(rel_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path.as_path(), text.as_bytes()).unwrap();
        tracing::info!("Wrote {path:?}");
    }

    pub fn remove_file(&self, rel_path: &str) {
        let path = Path::new(&self.path).join(rel_path);
        fs::remove_file(path.as_path()).unwrap();
        tracing::info!("Removed {path:?}");
    }

    pub fn remove_dir(&self, rel_path: &str) {
        let path = Path::new(&self.path).join(rel_path);
        fs::remove_dir(path.as_path()).unwrap();
        tracing::info!("Removed {path:?}");
    }

    pub fn create_dir_all(&self, rel_path: &str) {
        let path = Path::new(&self.path).join(rel_path);
        fs::create_dir_all(&path).unwrap();
        tracing::info!("Created dir {path:?}");
    }

    pub fn rename(&self, rel_from: &str, rel_to: &str) {
        let from = Path::new(&self.path).join(rel_from);
        let to = Path::new(&self.path).join(rel_to);
        fs::rename(&from, &to).unwrap();
        tracing::info!("Renamed from {from:?} to {to:?}");
    }

    pub fn create_symlink_dir(&self, rel_original: &str, rel_link: &str) {
        let original = Path::new(&self.path).join(rel_original);
        let link = Path::new(&self.path).join(rel_link);
        create_symlink_dir(original, link).unwrap();
    }

    pub fn create_symlink_file(&self, rel_original: &str, rel_link: &str) {
        let original = Path::new(&self.path).join(rel_original);
        let link = Path::new(&self.path).join(rel_link);
        create_symlink(original, link).unwrap()
    }
    pub fn create_relative_symlink_file(&self, point_to: &str, rel_link: &str) {
        let link = Path::new(&self.path).join(rel_link);
        create_symlink(point_to, link).unwrap()
    }
}

fn create_symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return std::os::unix::fs::symlink(original, link);

    #[cfg(target_os = "windows")]
    return std::os::windows::fs::symlink_dir(original, link);
}

fn create_symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return std::os::unix::fs::symlink(original, link);

    #[cfg(target_os = "windows")]
    return std::os::windows::fs::symlink_file(original, link);
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if self.keep {
            return;
        }

        let filetype = fs::symlink_metadata(&self.path).unwrap().file_type();
        let actual_path = filetype
            .is_symlink()
            .then(|| fs::read_link(&self.path).unwrap());

        if let Some(actual_path) = actual_path {
            remove_dir_all(actual_path.as_os_str().try_into().unwrap()).unwrap_or_else(|err| {
                panic!(
                    "failed to remove temporary link to directory {}: {err}",
                    actual_path.display()
                )
            })
        }

        remove_dir_all(&self.path).unwrap_or_else(|err| {
            panic!("failed to remove temporary directory {}: {err}", self.path)
        });
    }
}

#[cfg(not(windows))]
fn remove_dir_all(path: &str) -> io::Result<()> {
    fs::remove_dir_all(path)
}

#[cfg(windows)]
fn remove_dir_all(path: &str) -> io::Result<()> {
    for _ in 0..99 {
        if fs::remove_dir_all(path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10))
    }
    fs::remove_dir_all(path)
}
