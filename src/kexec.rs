use std::{
    convert::Infallible,
    ffi::CString,
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
};

const KEXEC_FILE_NO_INITRAMFS: libc::c_ulong = 0x00000004;
const LINUX_REBOOT_CMD_KEXEC: libc::c_int = 0x45584543;

pub(crate) struct KexecImage {
    kernel: File,
    initrd: Option<File>,
    cmdline: CString,
}

impl KexecImage {
    /// `kernel` and `initrd` must be readable regular-file descriptors.
    /// Payloads written through a writable memfd should be passed through
    /// `reopen_payload_readonly` first so `kexec_file_load` can deny writers.
    pub(crate) fn new(kernel: File, initrd: Option<File>, cmdline: &str) -> io::Result<Self> {
        let cmdline = CString::new(cmdline).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cmdline contains NUL byte")
        })?;
        Ok(Self {
            kernel,
            initrd,
            cmdline,
        })
    }

    pub(crate) fn load(&self) -> io::Result<()> {
        kexec_file_load(
            self.kernel.as_raw_fd(),
            self.initrd.as_ref().map(|file| file.as_raw_fd()),
            &self.cmdline,
        )
    }

    pub(crate) fn load_and_exec(&self) -> io::Result<Infallible> {
        self.load()?;
        exec_loaded_image()
    }
}

pub(crate) fn create_payload_memfd(name: &str) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memfd name contains NUL byte"))?;
    let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

pub(crate) fn reopen_payload_readonly(file: File) -> io::Result<File> {
    let path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let readonly = File::open(path)?;
    drop(file);
    Ok(readonly)
}

pub(crate) fn exec_loaded_image() -> io::Result<Infallible> {
    let rc = unsafe { libc::reboot(LINUX_REBOOT_CMD_KEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other(
        "reboot(LINUX_REBOOT_CMD_KEXEC) returned unexpectedly",
    ))
}

fn kexec_file_load(
    kernel_fd: libc::c_int,
    initrd_fd: Option<libc::c_int>,
    cmdline: &CString,
) -> io::Result<()> {
    let (initrd_fd, flags) = match initrd_fd {
        Some(fd) => (fd, 0),
        None => (-1, KEXEC_FILE_NO_INITRAMFS),
    };
    let cmdline_len = cmdline.as_bytes_with_nul().len();
    let rc = unsafe {
        libc::syscall(
            libc::SYS_kexec_file_load,
            libc::c_long::from(kernel_fd),
            libc::c_long::from(initrd_fd),
            cmdline_len as libc::c_ulong,
            cmdline.as_ptr(),
            flags,
        )
    };

    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
