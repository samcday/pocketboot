use std::{fs, io, mem, path::Path};

pub(crate) const INPUT: &str = "/dev/input";
const SYS_CLASS_INPUT: &str = "/sys/class/input";

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ: u8 = 2;

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const SYN_REPORT: u16 = 0x00;
pub(crate) const SYN_DROPPED: u16 = 0x03;
pub(crate) const KEY_VOLUMEDOWN: u16 = 0x72;
pub(crate) const KEY_VOLUMEUP: u16 = 0x73;
pub(crate) const KEY_POWER: u16 = 0x74;
pub(crate) const KEY_MAX: u16 = 0x2ff;
pub(crate) const KEY_BITS_BYTES: usize = KEY_POWER as usize / 8 + 1;
pub(crate) const KEY_BITMAP_BYTES: usize = (KEY_MAX as usize + 1) / 8;
pub(crate) const BTN_TOUCH: u16 = 0x14a;
pub(crate) const ABS_X: u16 = 0x00;
pub(crate) const ABS_Y: u16 = 0x01;
pub(crate) const ABS_MT_SLOT: u16 = 0x2f;
pub(crate) const ABS_MT_POSITION_X: u16 = 0x35;
pub(crate) const ABS_MT_POSITION_Y: u16 = 0x36;
pub(crate) const ABS_MT_TRACKING_ID: u16 = 0x39;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct InputEvent {
    pub(crate) time: libc::timeval,
    pub(crate) type_: u16,
    pub(crate) code: u16,
    pub(crate) value: i32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct InputAbsInfo {
    pub(crate) value: i32,
    pub(crate) minimum: i32,
    pub(crate) maximum: i32,
    pub(crate) fuzz: i32,
    pub(crate) flat: i32,
    pub(crate) resolution: i32,
}

pub(crate) fn test_bit(bits: &[u8], bit: u16) -> bool {
    let index = bit as usize / 8;
    let mask = 1 << (bit as usize % 8);
    bits.get(index).is_some_and(|byte| byte & mask != 0)
}

pub(crate) fn ioctl_read<T>(fd: i32, request: u64, value: &mut T) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value as *mut T) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn eviocgabs(abs: u16) -> u64 {
    ior(b'E', 0x40 + abs as u8, mem::size_of::<InputAbsInfo>())
}

pub(crate) fn eviocgbit(type_: u16, size: usize) -> u64 {
    ior(b'E', 0x20 + type_ as u8, size)
}

pub(crate) fn eviocgkey(size: usize) -> u64 {
    ior(b'E', 0x18, size)
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

/// Reads the key capability bitmap sysfs exports for an evdev node without
/// opening the node, so driver open() callbacks never run.
pub(crate) fn sysfs_event_has_key(event_name: &str, key: u16) -> bool {
    let path = Path::new(SYS_CLASS_INPUT)
        .join(event_name)
        .join("device/capabilities/key");
    fs::read_to_string(path).is_ok_and(|text| parse_sysfs_bitmap_has(&text, key as usize))
}

pub(crate) fn parse_sysfs_bitmap_has(text: &str, bit: usize) -> bool {
    parse_sysfs_bitmap_words_has(text, bit, libc::c_ulong::BITS as usize)
}

// The kernel prints one hex word per unsigned long, most significant first,
// and omits leading zero words.
fn parse_sysfs_bitmap_words_has(text: &str, bit: usize, word_bits: usize) -> bool {
    let Some(word) = text.split_whitespace().rev().nth(bit / word_bits) else {
        return false;
    };
    u64::from_str_radix(word, 16).is_ok_and(|word| (word >> (bit % word_bits)) & 1 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_evdev_ioctls() {
        assert_eq!(eviocgkey(KEY_BITMAP_BYTES), 0x80604518);
        assert_eq!(eviocgbit(EV_KEY, KEY_BITMAP_BYTES), 0x80604521);
        assert_eq!(eviocgbit(EV_KEY, KEY_BITS_BYTES), 0x800f4521);
        assert_eq!(eviocgabs(ABS_MT_POSITION_X), 0x80184575);
    }

    #[test]
    fn key_bitmap_covers_every_key() {
        assert_eq!(KEY_BITMAP_BYTES, 96);
    }

    #[test]
    fn tests_bitmap_bits() {
        let mut bits = [0u8; KEY_BITMAP_BYTES];
        bits[14] = 0x04;

        assert!(test_bit(&bits, KEY_VOLUMEDOWN));
        assert!(!test_bit(&bits, KEY_VOLUMEUP));
        assert!(!test_bit(&bits, KEY_MAX + 1));
    }

    #[test]
    fn parses_single_word_sysfs_bitmap() {
        assert!(parse_sysfs_bitmap_words_has("4\n", 2, 64));
        assert!(!parse_sysfs_bitmap_words_has("4\n", 1, 64));
        assert!(!parse_sysfs_bitmap_words_has("4\n", 66, 64));
    }

    #[test]
    fn parses_multi_word_sysfs_bitmap_with_64_bit_words() {
        // pm8941 RESIN plus pwrkey: KEY_VOLUMEDOWN (114) and KEY_POWER (116).
        let text = "14000000000000 0\n";

        assert!(parse_sysfs_bitmap_words_has(
            text,
            KEY_VOLUMEDOWN as usize,
            64
        ));
        assert!(parse_sysfs_bitmap_words_has(text, KEY_POWER as usize, 64));
        assert!(!parse_sysfs_bitmap_words_has(
            text,
            KEY_VOLUMEUP as usize,
            64
        ));
    }

    #[test]
    fn parses_multi_word_sysfs_bitmap_with_32_bit_words() {
        let text = "140000 0 0 0\n";

        assert!(parse_sysfs_bitmap_words_has(
            text,
            KEY_VOLUMEDOWN as usize,
            32
        ));
        assert!(parse_sysfs_bitmap_words_has(text, KEY_POWER as usize, 32));
        assert!(!parse_sysfs_bitmap_words_has(
            text,
            KEY_VOLUMEUP as usize,
            32
        ));
        assert!(!parse_sysfs_bitmap_words_has(
            "140000 0 0\n",
            KEY_VOLUMEDOWN as usize,
            32
        ));
    }

    #[test]
    fn empty_sysfs_bitmap_has_no_keys() {
        assert!(!parse_sysfs_bitmap_has("", KEY_VOLUMEDOWN as usize));
        assert!(!parse_sysfs_bitmap_has("0\n", KEY_VOLUMEDOWN as usize));
        assert!(!parse_sysfs_bitmap_has("0\n", 0));
    }

    #[test]
    fn rejects_malformed_sysfs_bitmap_words() {
        assert!(!parse_sysfs_bitmap_words_has("zz 0\n", 114, 64));
    }
}
