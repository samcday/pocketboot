use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    mem,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    slice, thread,
    time::{Duration, Instant},
};

use slint::platform::{
    Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
    },
};
use slint::{ComponentHandle, LogicalPosition, PhysicalSize};

slint::include_modules!();

const FB0: &str = "/dev/fb0";
const INPUT: &str = "/dev/input";
const UI_START_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_SLEEP: Duration = Duration::from_millis(16);

pub(crate) fn spawn() -> io::Result<thread::JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("pocketboot-ui".to_string())
        .spawn(|| {
            if let Err(err) = run() {
                tracing::warn!(error = %err, "UI thread exited");
            }
        })?;
    tracing::info!(thread = "pocketboot-ui", "UI thread spawned");
    Ok(handle)
}

fn run() -> Result<(), String> {
    let mut fb = Framebuffer::wait_open(FB0, UI_START_TIMEOUT)?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);

    slint::platform::set_platform(Box::new(PocketPlatform::new(window.clone())))
        .map_err(|err| format!("install Slint platform: {err}"))?;
    window.set_size(PhysicalSize::new(fb.width, fb.height));

    let main_window = MainWindow::new().map_err(|err| format!("create Slint window: {err}"))?;
    main_window.set_touch_x(fb.width as f32 / 2.0);
    main_window.set_touch_y(fb.height as f32 / 2.0);
    main_window
        .show()
        .map_err(|err| format!("show Slint window: {err}"))?;

    let mut touch = TouchInput::new();
    let mut pointer_down = false;
    tracing::info!(
        path = FB0,
        width = fb.width,
        height = fb.height,
        stride = fb.stride_pixels,
        "starting frankenSlint UI"
    );

    loop {
        slint::platform::update_timers_and_animations();

        for report in touch.poll(fb.width, fb.height) {
            main_window.set_touch_x(report.x);
            main_window.set_touch_y(report.y);

            let position = LogicalPosition::new(report.x, report.y);
            match report.kind {
                TouchKind::Down => {
                    pointer_down = true;
                    window.dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                }
                TouchKind::Move => {
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                }
                TouchKind::Up => {
                    if pointer_down {
                        pointer_down = false;
                        window.dispatch_event(WindowEvent::PointerReleased {
                            position,
                            button: PointerEventButton::Left,
                        });
                        window.dispatch_event(WindowEvent::PointerExited);
                    }
                }
            }
        }

        if !fb.draw_if_needed(&window) {
            thread::sleep(IDLE_SLEEP);
        }
    }
}

struct PocketPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl PocketPlatform {
    fn new(window: Rc<MinimalSoftwareWindow>) -> Self {
        Self {
            window,
            start: Instant::now(),
        }
    }
}

impl Platform for PocketPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window: Rc<dyn WindowAdapter> = self.window.clone();
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

struct Framebuffer {
    _file: File,
    pixels: *mut FbPixel,
    map_len: usize,
    width: u32,
    height: u32,
    stride_pixels: usize,
}

impl Framebuffer {
    fn wait_open(path: &str, timeout: Duration) -> Result<Self, String> {
        let start = Instant::now();
        loop {
            match Self::open(path) {
                Ok(fb) => return Ok(fb),
                Err(err) if start.elapsed() < timeout => {
                    tracing::debug!(path, error = %err, "framebuffer not ready");
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn open(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open {path}: {err}"))?;

        let fd = file.as_raw_fd();
        let mut fix = FbFixScreeninfo::default();
        let mut var = FbVarScreeninfo::default();
        ioctl_read(fd, FBIOGET_FSCREENINFO, &mut fix)
            .map_err(|err| format!("FBIOGET_FSCREENINFO {path}: {err}"))?;
        ioctl_read(fd, FBIOGET_VSCREENINFO, &mut var)
            .map_err(|err| format!("FBIOGET_VSCREENINFO {path}: {err}"))?;

        if var.bits_per_pixel != 32
            || var.red.offset != 16
            || var.red.length != 8
            || var.green.offset != 8
            || var.green.length != 8
            || var.blue.offset != 0
            || var.blue.length != 8
        {
            return Err(format!(
                "unsupported framebuffer format: {}bpp r{}:{} g{}:{} b{}:{}",
                var.bits_per_pixel,
                var.red.offset,
                var.red.length,
                var.green.offset,
                var.green.length,
                var.blue.offset,
                var.blue.length
            ));
        }

        if var.xres == 0 || var.yres == 0 || fix.line_length == 0 {
            return Err("framebuffer reported empty geometry".to_string());
        }

        let stride_pixels = fix.line_length as usize / mem::size_of::<FbPixel>();
        let visible_len = stride_pixels
            .checked_mul(var.yres as usize)
            .and_then(|len| len.checked_mul(mem::size_of::<FbPixel>()))
            .ok_or_else(|| "framebuffer geometry overflow".to_string())?;
        let map_len = usize::max(fix.smem_len as usize, visible_len);
        let pixels = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if pixels == libc::MAP_FAILED {
            return Err(format!("mmap {path}: {}", io::Error::last_os_error()));
        }

        Ok(Self {
            _file: file,
            pixels: pixels.cast(),
            map_len,
            width: var.xres,
            height: var.yres,
            stride_pixels,
        })
    }

    fn draw_if_needed(&mut self, window: &MinimalSoftwareWindow) -> bool {
        let len = self.stride_pixels * self.height as usize;
        let pixels = unsafe { slice::from_raw_parts_mut(self.pixels, len) };
        window.draw_if_needed(|renderer| {
            renderer.render(pixels, self.stride_pixels);
        })
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pixels.cast(), self.map_len);
        }
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
struct FbPixel(u32);

impl FbPixel {
    fn rgb(self) -> (u8, u8, u8) {
        (
            ((self.0 >> 16) & 0xff) as u8,
            ((self.0 >> 8) & 0xff) as u8,
            (self.0 & 0xff) as u8,
        )
    }
}

impl TargetPixel for FbPixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let (red, green, blue) = self.rgb();
        let inv_alpha = (u8::MAX - color.alpha) as u16;
        *self = Self::from_rgb(
            (red as u16 * inv_alpha / 255) as u8 + color.red,
            (green as u16 * inv_alpha / 255) as u8 + color.green,
            (blue as u16 * inv_alpha / 255) as u8 + color.blue,
        );
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xff00_0000 | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }
}

struct TouchInput {
    devices: Vec<TouchDevice>,
    ignored_devices: Vec<PathBuf>,
    next_scan: Instant,
}

impl TouchInput {
    fn new() -> Self {
        let mut input = Self {
            devices: Vec::new(),
            ignored_devices: Vec::new(),
            next_scan: Instant::now(),
        };
        input.rescan();
        input
    }

    fn poll(&mut self, width: u32, height: u32) -> Vec<TouchReport> {
        if Instant::now() >= self.next_scan {
            self.rescan();
        }

        let mut reports = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            match self.devices[index].poll(width, height, &mut reports) {
                Ok(()) => index += 1,
                Err(err) => {
                    tracing::warn!(path = %self.devices[index].path.display(), error = %err, "dropping touch input device");
                    self.devices.swap_remove(index);
                }
            }
        }
        reports
    }

    fn rescan(&mut self) {
        self.next_scan = Instant::now() + Duration::from_secs(1);
        let Ok(entries) = fs::read_dir(INPUT) else {
            return;
        };

        let mut present = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
            {
                continue;
            }
            present.push(path.clone());
            if self.devices.iter().any(|device| device.path == path) {
                continue;
            }
            if self.ignored_devices.iter().any(|ignored| *ignored == path) {
                continue;
            }
            match TouchDevice::open(&path) {
                Ok(device) => {
                    tracing::info!(path = %path.display(), "opened touch input device");
                    self.devices.push(device);
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "ignoring input device");
                    self.ignored_devices.push(path);
                }
            }
        }

        self.ignored_devices
            .retain(|ignored| present.iter().any(|path| path == ignored));
    }
}

struct TouchDevice {
    file: File,
    path: PathBuf,
    x_axis: Axis,
    y_axis: Axis,
    state: TouchState,
}

impl TouchDevice {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open: {err}"))?;
        let fd = file.as_raw_fd();

        let x_axis = query_axis(fd, ABS_MT_POSITION_X)
            .or_else(|_| query_axis(fd, ABS_X))
            .map_err(|err| format!("query x axis: {err}"))?;
        let y_axis = query_axis(fd, ABS_MT_POSITION_Y)
            .or_else(|_| query_axis(fd, ABS_Y))
            .map_err(|err| format!("query y axis: {err}"))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            x_axis,
            y_axis,
            state: TouchState::default(),
        })
    }

    fn poll(&mut self, width: u32, height: u32, reports: &mut Vec<TouchReport>) -> io::Result<()> {
        let mut buffer = [0u8; mem::size_of::<InputEvent>() * 32];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(bytes) => {
                    let whole_events = bytes / mem::size_of::<InputEvent>();
                    for chunk in buffer[..whole_events * mem::size_of::<InputEvent>()]
                        .chunks_exact(mem::size_of::<InputEvent>())
                    {
                        let event =
                            unsafe { ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
                        if let Some(report) =
                            self.state
                                .handle(event, self.x_axis, self.y_axis, width, height)
                        {
                            reports.push(report);
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Axis {
    code: u16,
    min: i32,
    max: i32,
}

impl Axis {
    fn scale(self, value: i32, size: u32) -> f32 {
        let span = (self.max - self.min).max(1) as f32;
        let value = ((value - self.min) as f32 / span).clamp(0.0, 1.0);
        value * (size.saturating_sub(1) as f32)
    }
}

#[derive(Default)]
struct TouchState {
    current_slot: usize,
    slots: [TouchSlot; MAX_TOUCH_SLOTS],
    legacy_x: Option<i32>,
    legacy_y: Option<i32>,
    legacy_down: bool,
    was_down: bool,
    last_point: Option<(i32, i32)>,
}

impl TouchState {
    fn handle(
        &mut self,
        event: InputEvent,
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Option<TouchReport> {
        match event.type_ {
            EV_ABS => self.handle_abs(event.code, event.value),
            EV_KEY if event.code == BTN_TOUCH => self.legacy_down = event.value != 0,
            EV_SYN if event.code == SYN_REPORT => {
                return self.report(x_axis, y_axis, width, height);
            }
            _ => {}
        }
        None
    }

    fn handle_abs(&mut self, code: u16, value: i32) {
        match code {
            ABS_MT_SLOT => {
                if (0..MAX_TOUCH_SLOTS as i32).contains(&value) {
                    self.current_slot = value as usize;
                }
            }
            ABS_MT_TRACKING_ID => {
                self.slots[self.current_slot].active = value >= 0;
            }
            ABS_MT_POSITION_X => self.slots[self.current_slot].x = Some(value),
            ABS_MT_POSITION_Y => self.slots[self.current_slot].y = Some(value),
            ABS_X => self.legacy_x = Some(value),
            ABS_Y => self.legacy_y = Some(value),
            _ => {}
        }
    }

    fn report(
        &mut self,
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Option<TouchReport> {
        let active_point = self.active_point(x_axis, y_axis);
        match (active_point, self.was_down) {
            (Some(point), false) => {
                self.was_down = true;
                self.last_point = Some(point);
                Some(TouchReport::new(
                    TouchKind::Down,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (Some(point), true) => {
                self.last_point = Some(point);
                Some(TouchReport::new(
                    TouchKind::Move,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (None, true) => {
                self.was_down = false;
                let point = self.last_point?;
                Some(TouchReport::new(
                    TouchKind::Up,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (None, false) => None,
        }
    }

    fn active_point(&self, x_axis: Axis, y_axis: Axis) -> Option<(i32, i32)> {
        if x_axis.code == ABS_MT_POSITION_X || y_axis.code == ABS_MT_POSITION_Y {
            for slot in self.slots {
                if slot.active {
                    if let (Some(x), Some(y)) = (slot.x, slot.y) {
                        return Some((x, y));
                    }
                }
            }
        }

        if self.legacy_down || self.was_down {
            if let (Some(x), Some(y)) = (self.legacy_x, self.legacy_y) {
                return Some((x, y));
            }
        }

        None
    }
}

#[derive(Clone, Copy, Default)]
struct TouchSlot {
    active: bool,
    x: Option<i32>,
    y: Option<i32>,
}

#[derive(Clone, Copy)]
struct TouchReport {
    kind: TouchKind,
    x: f32,
    y: f32,
}

impl TouchReport {
    fn new(
        kind: TouchKind,
        point: (i32, i32),
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            kind,
            x: x_axis.scale(point.0, width),
            y: y_axis.scale(point.1, height),
        }
    }
}

#[derive(Clone, Copy)]
enum TouchKind {
    Down,
    Move,
    Up,
}

fn query_axis(fd: i32, code: u16) -> io::Result<Axis> {
    let mut info = InputAbsInfo::default();
    ioctl_read(fd, eviocgabs(code), &mut info)?;
    if info.maximum <= info.minimum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty ABS axis range",
        ));
    }
    Ok(Axis {
        code,
        min: info.minimum,
        max: info.maximum,
    })
}

fn ioctl_read<T>(fd: i32, request: u64, value: &mut T) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value as *mut T) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn eviocgabs(abs: u16) -> u64 {
    ior(b'E', 0x40 + abs as u8, mem::size_of::<InputAbsInfo>())
}

fn ior(type_: u8, number: u8, size: usize) -> u64 {
    ioc(IOC_READ, type_, number, size)
}

fn ioc(direction: u8, type_: u8, number: u8, size: usize) -> u64 {
    ((direction as u64) << IOC_DIRSHIFT)
        | ((type_ as u64) << IOC_TYPESHIFT)
        | ((number as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOGET_FSCREENINFO: u64 = 0x4602;

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ: u8 = 2;

const MAX_TOUCH_SLOTS: usize = 10;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

#[repr(C)]
#[derive(Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
struct FbFixScreeninfo {
    id: [libc::c_char; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

impl Default for FbFixScreeninfo {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}
