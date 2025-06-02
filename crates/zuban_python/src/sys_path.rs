use std::{path::PathBuf, rc::Rc};

use vfs::{AbsPath, LocalFS, VfsHandler};

use crate::{PythonVersion, Settings};

pub(crate) fn create_sys_path(handler: &dyn VfsHandler, settings: &Settings) -> Vec<Rc<AbsPath>> {
    let mut sys_path = vec![];

    sys_path.extend(settings.prepended_site_packages.iter().cloned());

    if let Some(exe) = &settings.environment {
        // We cannot use cannonicalize here, because the path of the exe is often a venv path
        // that is a symlink to the actual exectuable. We however want the relative paths to
        // the symlink. Therefore cannonicalize only after getting the first dir
        let p = site_packages_path_from_venv(exe, settings.python_version);
        sys_path.push(
            handler.unchecked_abs_path(
                p.into_os_string().into_string().expect(
                    "Should never happen, because we only put together valid unicode paths",
                ),
            ),
        );
    } else {
        // TODO use a real sys path
        //"../typeshed/stubs".into(),
        //"/usr/lib/python3/dist-packages".into(),
        //"/usr/local/lib/python3.8/dist-packages/pip-20.0.2-py3.8.egg".into(),
        //"/usr/lib/python3.8".into(),
        //"/home/<user>/.local/lib/python3.8/site-packages".into(),
        //"/usr/local/lib/python3.8/dist-packages".into(),
    }
    sys_path
}

fn site_packages_path_from_venv(environment: &AbsPath, version: PythonVersion) -> PathBuf {
    let lib = environment.as_ref().join("lib");

    let expected_path = lib
        .join(format!("python{}.{}", version.major, version.minor))
        .join("site-packages");

    if expected_path.exists() {
        return expected_path;
    }
    // Since the path we wanted doesn't exist, we fall back to trying to find a folder in the lib,
    // because we are probably not always using the correct PythonVersion.
    match lib.read_dir() {
        Ok(dir) => {
            for path_in_dir in dir.flatten() {
                let n = path_in_dir.file_name();
                if n.as_encoded_bytes().starts_with(b"python") {
                    return lib.join(n).join("site-packages");
                }
            }
        }
        Err(err) => {
            tracing::error!("Expected {lib:?} to be a directory: {err}");
        }
    }
    expected_path
}

pub(crate) fn typeshed_path_from_executable() -> Rc<AbsPath> {
    let executable = std::env::current_exe().expect(
        "Cannot access the path of the current executable, you need to provide \
                 a typeshed path in that case.",
    );
    const NEEDS_PARENTS: &str = "The executable is expected to be relative to the typeshed path";
    let lib_folder = executable
        .parent()
        .expect(NEEDS_PARENTS)
        .parent()
        .expect(NEEDS_PARENTS)
        .join("lib");
    // The lib folder typically contains a Python specific folder called "python3.8" or
    // python3.13", corresponding to the Python version. Here we try to find the package.
    for folder in lib_folder.read_dir().unwrap_or_else(|err| {
        panic!(
            "The Python environment lib folder {lib_folder:?} should be readable ({err}).
                You might want to set ZUBAN_TYPESHED."
        )
    }) {
        let folder = folder.unwrap_or_else(|err| {
            panic!("The lib folder {lib_folder:?} should be readable ({err})")
        });
        let p = folder.path();
        let typeshed_path = p.join("site-packages").join("zuban").join("typeshed");
        if typeshed_path.exists() {
            return LocalFS::without_watcher().abs_path_from_current_dir(
                typeshed_path
                    .into_os_string()
                    .into_string()
                    .expect("Expected the typeshed path to be UTF-8"),
            );
        }
    }
    panic!("Did not find a typeshed folder in {lib_folder:?}")
}
