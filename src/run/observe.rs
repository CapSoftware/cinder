//! Wrapper-free observation of Cargo's selected compiler process.
//!
//! On macOS, Cinder can inspect the direct children of the Cargo process it
//! started. Observation is read-only and best-effort: if a compiler exits too
//! quickly, process metadata is unavailable, or the operating-system contract
//! changes, no recipe is published and the next edit remains Cargo-owned.

use super::CompilerRecipe;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

pub(super) struct CompilerObserver {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<CompilerRecipe>>>,
}

impl CompilerObserver {
    pub(super) fn start(cargo_pid: u32, enabled: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if enabled {
            #[cfg(target_os = "macos")]
            {
                i32::try_from(cargo_pid).ok().and_then(|cargo_pid| {
                    let stop = Arc::clone(&stop);
                    std::thread::Builder::new()
                        .name("cinder-compiler-observer".to_owned())
                        .spawn(move || macos::observe_compilers(cargo_pid, &stop))
                        .ok()
                })
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = cargo_pid;
                None
            }
        } else {
            None
        };
        Self { stop, handle }
    }

    pub(super) fn finish(mut self) -> Vec<CompilerRecipe> {
        self.stop.store(true, Ordering::Release);
        let recipes = self
            .handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        if std::env::var_os(super::TRACE_RUN).is_some() && !recipes.is_empty() {
            eprintln!(
                "    Cinder trace: observed {} compiler process candidate(s)",
                recipes.len()
            );
        }
        recipes
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{
        collections::BTreeSet,
        ffi::{OsString, c_int, c_void},
        mem::{MaybeUninit, size_of},
        os::unix::ffi::OsStringExt,
        path::PathBuf,
        ptr, thread,
        time::Duration,
    };

    const MAX_CHILDREN: usize = 4_096;
    const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
    const MAX_PATH_BYTES: usize = 4 * 1024;
    const CTL_KERN: c_int = 1;
    const KERN_PROCARGS2: c_int = 49;
    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const MAXPATHLEN: usize = 1024;

    struct ObservedInvocation {
        executable: PathBuf,
        arguments: Vec<OsString>,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VInfoStat {
        device: u32,
        mode: u16,
        links: u16,
        inode: u64,
        user: u32,
        group: u32,
        access_time: i64,
        access_nanoseconds: i64,
        modified_time: i64,
        modified_nanoseconds: i64,
        changed_time: i64,
        changed_nanoseconds: i64,
        birth_time: i64,
        birth_nanoseconds: i64,
        size: i64,
        blocks: i64,
        block_size: i32,
        flags: u32,
        generation: u32,
        raw_device: u32,
        spare: [i64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VnodeInfo {
        stat: VInfoStat,
        kind: c_int,
        padding: c_int,
        filesystem: [i32; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VnodeInfoPath {
        info: VnodeInfo,
        path: [u8; MAXPATHLEN],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessVnodePathInfo {
        current: VnodeInfoPath,
        root: VnodeInfoPath,
    }

    unsafe extern "C" {
        fn proc_listchildpids(ppid: c_int, buffer: *mut c_void, size: c_int) -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            argument: u64,
            buffer: *mut c_void,
            size: c_int,
        ) -> c_int;
        fn sysctl(
            name: *mut c_int,
            name_length: u32,
            old_value: *mut c_void,
            old_length: *mut usize,
            new_value: *mut c_void,
            new_length: usize,
        ) -> c_int;
    }

    pub(super) fn observe_compilers(cargo_pid: c_int, stop: &AtomicBool) -> Vec<CompilerRecipe> {
        let mut captured = BTreeSet::new();
        let mut recipes = Vec::new();
        loop {
            observe_children(cargo_pid, &mut captured, &mut recipes);
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_micros(250));
        }
        recipes
    }

    fn observe_children(
        cargo_pid: c_int,
        captured: &mut BTreeSet<c_int>,
        recipes: &mut Vec<CompilerRecipe>,
    ) {
        let mut children = [0_i32; MAX_CHILDREN];
        let size = c_int::try_from(size_of_val(&children)).unwrap_or(c_int::MAX);
        // SAFETY: `children` is writable for exactly `size` bytes. Failure is a
        // normal observation miss and cannot affect Cargo.
        let count =
            unsafe { proc_listchildpids(cargo_pid, children.as_mut_ptr().cast::<c_void>(), size) };
        let Ok(count) = usize::try_from(count) else {
            return;
        };
        let count = count.min(children.len());
        for &pid in &children[..count] {
            if pid <= 0 || captured.contains(&pid) {
                continue;
            }
            if let Some(recipe) = compiler_recipe(pid) {
                captured.insert(pid);
                recipes.push(recipe);
            }
        }
    }

    fn compiler_recipe(pid: c_int) -> Option<CompilerRecipe> {
        let process_path_before = process_path(pid)?;
        let working_directory = process_working_directory(pid)?;
        let invocation = process_arguments(pid)?;
        // libproc queries are independent snapshots. A short-lived compiler
        // can exit (and its PID can be reused) between them, so never combine
        // arguments and a working directory observed across an executable
        // transition. The exec path embedded in KERN_PROCARGS2 preserves the
        // invoked rustup hard-link/symlink name (`rustc` rather than whichever
        // proxy path proc_pidpath happens to report) and is therefore the path
        // that must be replayed.
        if process_path(pid)? != process_path_before {
            return None;
        }
        CompilerRecipe::from_observed(
            invocation.executable,
            working_directory,
            invocation.arguments,
        )
    }

    fn process_path(pid: c_int) -> Option<PathBuf> {
        let mut path = [0_u8; MAX_PATH_BYTES];
        // SAFETY: `path` is writable for the supplied length. The API returns
        // zero or a negative value when the process disappears or is denied.
        let length = unsafe {
            proc_pidpath(
                pid,
                path.as_mut_ptr().cast::<c_void>(),
                u32::try_from(path.len()).ok()?,
            )
        };
        let length = usize::try_from(length).ok()?.min(path.len());
        let length = path[..length]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(length);
        (length > 0).then(|| PathBuf::from(OsString::from_vec(path[..length].to_vec())))
    }

    fn process_working_directory(pid: c_int) -> Option<PathBuf> {
        let mut information = MaybeUninit::<ProcessVnodePathInfo>::uninit();
        let expected = c_int::try_from(size_of::<ProcessVnodePathInfo>()).ok()?;
        // SAFETY: the uninitialized allocation is writable for `expected`
        // bytes and is read only when libproc reports the complete structure.
        let written = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDVNODEPATHINFO,
                0,
                information.as_mut_ptr().cast::<c_void>(),
                expected,
            )
        };
        if written != expected {
            return None;
        }
        // SAFETY: the exact structure size was initialized above.
        let information = unsafe { information.assume_init() };
        let path = information.current.path;
        let length = path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path.len());
        (length > 0).then(|| PathBuf::from(OsString::from_vec(path[..length].to_vec())))
    }

    fn process_arguments(pid: c_int) -> Option<ObservedInvocation> {
        let mut bytes = vec![0_u8; MAX_ARGUMENT_BYTES];
        let mut length = bytes.len();
        let mut name = [CTL_KERN, KERN_PROCARGS2, pid];
        // SAFETY: `bytes` is writable for `length` bytes and the remaining
        // pointers describe the documented read-only KERN_PROCARGS2 query.
        let status = unsafe {
            sysctl(
                name.as_mut_ptr(),
                u32::try_from(name.len()).ok()?,
                bytes.as_mut_ptr().cast::<c_void>(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if status != 0 || length < size_of::<c_int>() || length >= bytes.len() {
            return None;
        }
        bytes.truncate(length);
        parse_process_arguments(&bytes)
    }

    fn parse_process_arguments(bytes: &[u8]) -> Option<ObservedInvocation> {
        let argument_count = c_int::from_ne_bytes(bytes[..size_of::<c_int>()].try_into().ok()?);
        let argument_count = usize::try_from(argument_count).ok()?;
        if argument_count > bytes.len() {
            return None;
        }
        let mut offset = size_of::<c_int>();

        // KERN_PROCARGS2 starts with the exact executable path used for this
        // invocation followed by padding. Keep it: proc_pidpath can resolve a
        // rustup proxy through a different hard-link name such as `cargo`.
        let executable_end = next_zero(bytes, offset)?;
        let executable = PathBuf::from(OsString::from_vec(bytes[offset..executable_end].to_vec()));
        offset = executable_end.checked_add(1)?;
        while bytes.get(offset) == Some(&0) {
            offset += 1;
        }

        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            let end = next_zero(bytes, offset)?;
            arguments.push(OsString::from_vec(bytes[offset..end].to_vec()));
            offset = end.checked_add(1)?;
        }
        if arguments.first().is_none_or(|argument| argument.is_empty()) {
            return None;
        }
        arguments.remove(0);
        Some(ObservedInvocation {
            executable,
            arguments,
        })
    }

    fn next_zero(bytes: &[u8], offset: usize) -> Option<usize> {
        bytes
            .get(offset..)?
            .iter()
            .position(|byte| *byte == 0)
            .and_then(|position| offset.checked_add(position))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{fs, process::Command, time::SystemTime};

        #[test]
        fn observes_a_selected_compiler_without_a_wrapper() {
            let unique = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "cinder-process-observer-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let mut child = Command::new("/bin/sh")
                .current_dir(&root)
                .arg("-c")
                .arg(
                    "/bin/sh -c 'sleep 0.15' rustc --crate-name observed \
                     --crate-type lib --emit metadata --out-dir /tmp & wait",
                )
                .spawn()
                .unwrap();
            let observer = CompilerObserver::start(child.id(), true);
            assert!(child.wait().unwrap().success());
            let recipes = observer.finish();
            assert_eq!(recipes.len(), 1);
            let recipe = &recipes[0];
            assert_eq!(recipe.executable, PathBuf::from("/bin/sh"));
            assert_eq!(
                fs::canonicalize(&recipe.working_directory).unwrap(),
                fs::canonicalize(&root).unwrap()
            );
            assert!(
                recipe
                    .arguments
                    .windows(2)
                    .any(|values| values == ["--crate-name", "observed"])
            );
            assert!(recipe.environment.is_empty());
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn parses_the_invoked_executable_without_substituting_a_proxy_path() {
            let arguments = ["rustc", "--crate-name", "observed", "--emit", "metadata"];
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(arguments.len() as c_int).to_ne_bytes());
            bytes.extend_from_slice(b"/toolchain/bin/rustc\0\0");
            for argument in arguments {
                bytes.extend_from_slice(argument.as_bytes());
                bytes.push(0);
            }
            // KERN_PROCARGS2 may append envp and Apple auxiliary strings here.
            // Cinder deliberately stops after argc rather than treating that
            // unbounded tail as an exact compiler environment.
            bytes.extend_from_slice(b"CARGO_PKG_VERSION=9.9.9\0IGNORED=value\0");

            let invocation = parse_process_arguments(&bytes).unwrap();
            assert_eq!(invocation.executable, PathBuf::from("/toolchain/bin/rustc"));
            assert_eq!(
                invocation.arguments,
                arguments[1..]
                    .iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            );
        }
    }
}
