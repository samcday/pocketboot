use std::{
    fs, io,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const IFCONFIG: &str = "/sbin/ifconfig";
const USBIP_PORT: u16 = 3240;
const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const ST_OK: u32 = 0x00;
const ST_NA: u32 = 0x01;
const ST_DEV_BUSY: u32 = 0x02;
const ST_DEV_ERR: u32 = 0x03;
const ST_NODEV: u32 = 0x04;
const ST_ERROR: u32 = 0x05;
const SDEV_ST_AVAILABLE: u32 = 0x01;
const SDEV_ST_USED: u32 = 0x02;
const SDEV_ST_ERROR: u32 = 0x03;
const SYSFS_PATH_MAX: usize = 256;
const SYSFS_BUS_ID_SIZE: usize = 32;
const USBIP_USB_DEVICE_SIZE: usize = 312;
const VUDC_NAME: &str = "usbip-vudc.0";
const VUDC_CLASS: &str = "/sys/class/udc/usbip-vudc.0";

pub(crate) fn spawn() -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("pocketboot-qemu".to_string())
        .spawn(|| {
            if let Err(err) = run() {
                tracing::warn!(error = ?err, "QEMU USB/IP service failed");
            }
        })
}

fn run() -> io::Result<()> {
    tracing::warn!("starting QEMU USB/IP support");
    configure_network()?;
    run_usbip_server()
}

fn configure_network() -> io::Result<()> {
    run_ifconfig(["lo", "127.0.0.1", "up"])?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match run_ifconfig(["eth0", "10.0.2.15", "netmask", "255.255.255.0", "up"]) {
            Ok(()) => {
                tracing::warn!("QEMU network configured on eth0");
                return Ok(());
            }
            Err(err) if Instant::now() < deadline => {
                tracing::debug!(error = ?err, "waiting for QEMU eth0");
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}

fn run_ifconfig<const N: usize>(args: [&str; N]) -> io::Result<()> {
    let status = Command::new(IFCONFIG)
        .args(args)
        .stdin(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{IFCONFIG} failed with {status}")))
    }
}

fn run_usbip_server() -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", USBIP_PORT))?;
    tracing::warn!(port = USBIP_PORT, "QEMU USB/IP vUDC server listening");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = thread::Builder::new()
                    .name("pocketboot-usbip".to_string())
                    .spawn(|| handle_usbip_client(stream))
                {
                    tracing::warn!(error = ?err, "failed to spawn USB/IP client thread");
                }
            }
            Err(err) => tracing::warn!(error = ?err, "failed to accept USB/IP connection"),
        }
    }
    Ok(())
}

fn handle_usbip_client(mut stream: TcpStream) {
    if let Err(err) = handle_usbip_request(&mut stream) {
        tracing::warn!(error = ?err, "USB/IP request failed");
    }
}

fn handle_usbip_request(stream: &mut TcpStream) -> io::Result<()> {
    let common = read_op_common(stream)?;
    if common.version != USBIP_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "USB/IP version mismatch: got 0x{:04x}, expected 0x{USBIP_VERSION:04x}",
                common.version
            ),
        ));
    }

    match common.code {
        OP_REQ_DEVLIST => reply_devlist(stream),
        OP_REQ_IMPORT => reply_import(stream),
        code => {
            tracing::warn!(code = format!("0x{code:04x}"), "unknown USB/IP op");
            send_op_common(stream, 0, ST_ERROR)
        }
    }
}

fn reply_devlist(stream: &mut TcpStream) -> io::Result<()> {
    let device = exported_device(Duration::from_secs(10)).ok();
    let listed = device
        .as_ref()
        .filter(|device| device.status != SDEV_ST_USED);
    send_op_common(stream, OP_REP_DEVLIST, ST_OK)?;
    write_u32(stream, u32::from(listed.is_some()))?;
    if let Some(device) = listed {
        write_usb_device(stream, device)?;
    }
    Ok(())
}

fn reply_import(stream: &mut TcpStream) -> io::Result<()> {
    let mut busid = [0; SYSFS_BUS_ID_SIZE];
    stream.read_exact(&mut busid)?;
    let requested = nul_string(&busid);
    let device = match exported_device(Duration::from_secs(10)) {
        Ok(device) => device,
        Err(err) => {
            tracing::warn!(requested, error = ?err, "requested vUDC is not available");
            return send_op_common(stream, OP_REP_IMPORT, ST_NODEV);
        }
    };

    if requested != device.busid {
        tracing::warn!(
            requested,
            busid = device.busid,
            "requested USB/IP busid not found"
        );
        return send_op_common(stream, OP_REP_IMPORT, ST_NODEV);
    }

    match device.status {
        SDEV_ST_AVAILABLE => {}
        SDEV_ST_USED => return send_op_common(stream, OP_REP_IMPORT, ST_DEV_BUSY),
        SDEV_ST_ERROR => return send_op_common(stream, OP_REP_IMPORT, ST_DEV_ERR),
        status => {
            tracing::warn!(status, "unknown USB/IP vUDC status");
            return send_op_common(stream, OP_REP_IMPORT, ST_NA);
        }
    }

    stream.set_nodelay(true)?;
    let fd = stream.as_raw_fd();
    if let Err(err) = fs::write(device.path.join("usbip_sockfd"), format!("{fd}\n")) {
        tracing::warn!(error = ?err, "failed to hand USB/IP socket to vUDC");
        return send_op_common(stream, OP_REP_IMPORT, ST_NA);
    }
    send_op_common(stream, OP_REP_IMPORT, ST_OK)?;
    write_usb_device(stream, &device)
}

fn exported_device(timeout: Duration) -> io::Result<UsbIpDevice> {
    let deadline = Instant::now() + timeout;
    loop {
        match read_exported_device() {
            Ok(device) => return Ok(device),
            Err(err) if Instant::now() < deadline => {
                tracing::debug!(error = ?err, "waiting for usbip-vudc export");
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}

fn read_exported_device() -> io::Result<UsbIpDevice> {
    let udc = fs::canonicalize(VUDC_CLASS)?;
    let path = udc
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "vUDC platform device not found"))?
        .to_path_buf();
    let descriptor = fs::read(path.join("dev_desc"))?;
    let descriptor = DeviceDescriptor::parse(&descriptor)?;
    let status = read_trimmed(path.join("usbip_status"))?
        .parse()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid usbip_status: {err}"),
            )
        })?;
    let speed = usb_speed(&read_trimmed(udc.join("current_speed"))?);

    Ok(UsbIpDevice {
        path,
        busid: VUDC_NAME.to_string(),
        status,
        speed,
        descriptor,
    })
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    fs::read_to_string(path).map(|value| value.trim().to_string())
}

fn usb_speed(value: &str) -> u32 {
    match value {
        "low-speed" => 1,
        "full-speed" => 2,
        "high-speed" => 3,
        "wireless" => 4,
        "super-speed" => 5,
        "super-speed-plus" => 6,
        _ => 0,
    }
}

#[derive(Debug)]
struct OpCommon {
    version: u16,
    code: u16,
}

fn read_op_common(stream: &mut TcpStream) -> io::Result<OpCommon> {
    let mut bytes = [0; 8];
    stream.read_exact(&mut bytes)?;
    Ok(OpCommon {
        version: u16::from_be_bytes([bytes[0], bytes[1]]),
        code: u16::from_be_bytes([bytes[2], bytes[3]]),
    })
}

fn send_op_common(stream: &mut TcpStream, code: u16, status: u32) -> io::Result<()> {
    write_u16(stream, USBIP_VERSION)?;
    write_u16(stream, code)?;
    write_u32(stream, status)
}

fn write_usb_device(stream: &mut TcpStream, device: &UsbIpDevice) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(USBIP_USB_DEVICE_SIZE);
    push_padded_string(&mut bytes, &path_string(&device.path)?, SYSFS_PATH_MAX)?;
    push_padded_string(&mut bytes, &device.busid, SYSFS_BUS_ID_SIZE)?;
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&device.speed.to_be_bytes());
    bytes.extend_from_slice(&device.descriptor.id_vendor.to_be_bytes());
    bytes.extend_from_slice(&device.descriptor.id_product.to_be_bytes());
    bytes.extend_from_slice(&device.descriptor.bcd_device.to_be_bytes());
    bytes.push(device.descriptor.device_class);
    bytes.push(device.descriptor.device_sub_class);
    bytes.push(device.descriptor.device_protocol);
    bytes.push(0);
    bytes.push(device.descriptor.num_configurations);
    bytes.push(0);
    debug_assert_eq!(bytes.len(), USBIP_USB_DEVICE_SIZE);
    stream.write_all(&bytes)
}

fn write_u16(stream: &mut TcpStream, value: u16) -> io::Result<()> {
    stream.write_all(&value.to_be_bytes())
}

fn write_u32(stream: &mut TcpStream, value: u32) -> io::Result<()> {
    stream.write_all(&value.to_be_bytes())
}

fn push_padded_string(buffer: &mut Vec<u8>, value: &str, size: usize) -> io::Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() >= size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("USB/IP string is too long: {value}"),
        ));
    }
    buffer.extend_from_slice(bytes);
    buffer.resize(buffer.len() + size - bytes.len(), 0);
    Ok(())
}

fn path_string(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "vUDC path is not valid UTF-8"))
}

fn nul_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

#[derive(Debug)]
struct UsbIpDevice {
    path: PathBuf,
    busid: String,
    status: u32,
    speed: u32,
    descriptor: DeviceDescriptor,
}

#[derive(Debug)]
struct DeviceDescriptor {
    device_class: u8,
    device_sub_class: u8,
    device_protocol: u8,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    num_configurations: u8,
}

impl DeviceDescriptor {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 18 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short USB device descriptor",
            ));
        }
        Ok(Self {
            device_class: bytes[4],
            device_sub_class: bytes[5],
            device_protocol: bytes[6],
            id_vendor: u16::from_le_bytes([bytes[8], bytes[9]]),
            id_product: u16::from_le_bytes([bytes[10], bytes[11]]),
            bcd_device: u16::from_le_bytes([bytes[12], bytes[13]]),
            num_configurations: bytes[17],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_descriptor() {
        let descriptor = DeviceDescriptor::parse(&[
            18, 1, 0, 2, 0xef, 0x02, 0x01, 64, 0x6b, 0x1d, 0x04, 0x01, 0x00, 0x01, 1, 2, 3, 1,
        ])
        .unwrap();

        assert_eq!(descriptor.device_class, 0xef);
        assert_eq!(descriptor.device_sub_class, 0x02);
        assert_eq!(descriptor.device_protocol, 0x01);
        assert_eq!(descriptor.id_vendor, 0x1d6b);
        assert_eq!(descriptor.id_product, 0x0104);
        assert_eq!(descriptor.bcd_device, 0x0100);
        assert_eq!(descriptor.num_configurations, 1);
    }

    #[test]
    fn pads_usbip_strings() {
        let mut bytes = Vec::new();

        push_padded_string(&mut bytes, "abc", 8).unwrap();

        assert_eq!(&bytes, b"abc\0\0\0\0\0");
    }
}
