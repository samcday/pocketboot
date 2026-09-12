#![cfg_attr(test, allow(dead_code))]

use core::{
    ptr, slice,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::fdt::{
    self, FDT_BEGIN_NODE, FDT_END, FDT_END_NODE, FDT_NOP, FDT_PROP, Node, Reader, Writer,
};

const MAX_DTB_SIZE: usize = 512 * 1024;
const MAX_CPUS: usize = 4;
const MAX_DEPTH: usize = 32;

// Fixed ABI shared with arm64 spin-table CPU operations and the kexec loader.
const SPIN_TABLE_COMPATIBLE: &[u8] = b"pocketboot,spin-table-v1";
const SPIN_TABLE_SIZE: usize = 4096;
const SPIN_TABLE_ENTRY_OFFSET: usize = 0x100;
const SPIN_TABLE_SLOTS_OFFSET: usize = 0x400;
const SPIN_TABLE_SLOT_STRIDE: usize = 0x80;
const SPIN_TABLE_REQUEST_OFFSET: usize = 8;
const SPIN_TABLE_ACK_OFFSET: usize = 0x40;
const SPIN_TABLE_ENTRY_EL_OFFSET: usize = 0x48;
const SPIN_TABLE_CPUECTLR_OFFSET: usize = 0x50;
const SPIN_TABLE_DIAG_TAG_OFFSET: usize = 0xb0;
const SPIN_TABLE_DIAG_TAG: &[u8] = b"PBSDIAG1";
const CPUECTLR_SMPEN: u64 = 1 << 6;
const STARTUP_TIMEOUT_US: u64 = 1_000_000;

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(include_str!("spin_table.S"));

#[repr(align(64))]
#[allow(dead_code)]
struct DtbScratch([u8; MAX_DTB_SIZE]);

static mut DTB_SCRATCH: DtbScratch = DtbScratch([0; MAX_DTB_SIZE]);

pub mod uart {
    use super::*;

    const UARTDM_DMEN: usize = 0x03c;
    const UARTDM_TX_SINGLE_CHAR: u32 = 1 << 4;
    const UARTDM_TX_BAM_ENABLE: u32 = 1 << 2;
    const UARTDM_CR: usize = 0x0a8;
    const UARTDM_CR_RESET_TX: u32 = 2 << 4;
    const UARTDM_CR_TX_ENABLE: u32 = 1 << 2;
    const UARTDM_SR: usize = 0x0a4;
    const UARTDM_SR_TX_READY: u32 = 1 << 2;
    const UARTDM_TF: usize = 0x100;
    const TX_READY_TIMEOUT: usize = 100_000;

    static UART_BASE: AtomicUsize = AtomicUsize::new(0);

    pub fn set_base(base: usize) {
        // Retain the firmware's pin mux, clock and baud rate. The FIFO writes
        // below require UARTDM v1.4 single-character mode, which lk2nd/Linux
        // need not have left enabled. Only reconfigure the transmit side.
        write32(base + UARTDM_CR, UARTDM_CR_RESET_TX);
        let dmen = read32(base + UARTDM_DMEN);
        write32(
            base + UARTDM_DMEN,
            (dmen & !UARTDM_TX_BAM_ENABLE) | UARTDM_TX_SINGLE_CHAR,
        );
        write32(base + UARTDM_CR, UARTDM_CR_TX_ENABLE);
        dsb_sy();
        UART_BASE.store(base, Ordering::Relaxed);
    }

    pub fn writeln(message: &str) {
        write_str(message);
        write_byte(b'\r');
        write_byte(b'\n');
    }

    pub fn write_str(message: &str) {
        for byte in message.bytes() {
            if byte == b'\n' {
                write_byte(b'\r');
            }
            write_byte(byte);
        }
    }

    pub fn write_hex64(value: u64) {
        write_str("0x");
        let mut started = false;
        for shift in (0..64).step_by(4).rev() {
            let nibble = ((value >> shift) & 0xf) as u8;
            if nibble != 0 || started || shift == 0 {
                started = true;
                write_byte(if nibble < 10 {
                    b'0' + nibble
                } else {
                    b'a' + (nibble - 10)
                });
            }
        }
    }

    fn write_byte(byte: u8) {
        #[cfg(all(target_os = "none", feature = "debug-ram-trace"))]
        crate::ram_trace::write_byte(byte);
        let base = UART_BASE.load(Ordering::Relaxed);
        if base == 0 {
            return;
        }

        for _ in 0..TX_READY_TIMEOUT {
            if read32(base + UARTDM_SR) & UARTDM_SR_TX_READY != 0 {
                write32(base + UARTDM_TF, byte as u32);
                return;
            }
            core::hint::spin_loop();
        }
    }
}

pub fn early_init(fdt: usize) {
    if let Ok(reader) = unsafe { Reader::from_ptr(fdt) } {
        if let Ok(base) = find_uart_base(&reader) {
            #[cfg(all(target_os = "none", feature = "debug-ram-trace"))]
            crate::ram_trace::stage(4);
            uart::set_base(base as usize);
            #[cfg(all(target_os = "none", feature = "debug-ram-trace"))]
            crate::ram_trace::stage(5);
        }
    }

    uart::writeln("msm8916 preboot");
}

pub fn prepare_fdt(fdt: usize, payload: usize, payload_size: usize) -> Option<usize> {
    match prepare_fdt_inner(fdt, payload, payload_size) {
        Ok(fdt) => Some(fdt),
        Err(error) => {
            uart::write_str("msm8916: setup failed: ");
            uart::writeln(error.message());
            None
        }
    }
}

fn prepare_fdt_inner(fdt: usize, payload: usize, payload_size: usize) -> Result<usize> {
    if read_mpidr() != 0 || !cache_mmu_off() || timer_frequency() == 0 {
        return Err(Error::EntryState);
    }
    uart::write_str("msm8916: primary CurrentEL=");
    uart::write_hex64(current_el());
    uart::writeln("");
    let primary_cpuectlr = read_cpuectlr();
    uart::write_str("msm8916: primary CPUECTLR=");
    uart::write_hex64(primary_cpuectlr);
    uart::writeln("");
    require_coherency(primary_cpuectlr)?;
    let reader = unsafe { Reader::from_ptr(fdt) }?;
    uart::writeln("msm8916: find reservation");
    let spin_table = find_spin_table(&reader)?;
    uart::writeln("msm8916: validate memory");
    validate_memory(&reader, spin_table, payload, payload_size)?;
    uart::writeln("msm8916: collect CPUs");
    let cpus = collect_cpus(&reader)?;

    // Finish all fallible DT work before releasing a secondary or touching
    // its ACC. Keep WFI idle; power collapse needs a separate resume protocol.
    let patched = unsafe {
        let scratch = ptr::addr_of_mut!(DTB_SCRATCH).cast::<u8>();
        slice::from_raw_parts_mut(scratch, MAX_DTB_SIZE)
    };
    uart::writeln("msm8916: patch FDT");
    let patched = patch_fdt(&reader, patched, spin_table)?;
    uart::writeln("msm8916: clean FDT");
    clean_dcache_range(patched.as_ptr() as usize, patched.len());

    if cpus.mode == CpuBootMode::Resident {
        // The outgoing kernel has already returned all secondaries to the
        // immutable page. Validate its handoff without writing even a byte:
        // those CPUs may currently be executing the code we are inspecting.
        let prefix = unsafe { slice::from_raw_parts(spin_table.addr as *const u8, 0xb8) };
        validate_resident_descriptor(prefix)?;
        validate_parked_slots(spin_table, current_el(), read64)?;
        if has_coherency_diagnostics(prefix) {
            for cpu in 1..MAX_CPUS as u32 {
                report_secondary_coherency(spin_table, cpu)?;
            }
        } else {
            uart::writeln("msm8916: older resident page has no CPUECTLR snapshots");
        }
        uart::writeln("msm8916: reusing acknowledged resident CPUs");
        return Ok(patched.as_ptr() as usize);
    }

    uart::writeln("msm8916: install resident code");
    init_spin_table(spin_table)?;
    uart::writeln("msm8916: configure SCM entry");
    scm::set_boot_addr_mc(
        spin_table.code_addr(),
        scm::BOOT_MC_FLAG_AARCH64 | scm::BOOT_MC_FLAG_COLDBOOT,
    )?;

    for cpu in cpus.as_slice() {
        if cpu.reg == 0 {
            continue;
        }
        uart::write_str("msm8916: boot cpu");
        uart::write_hex64(cpu.reg as u64);
        uart::write_str(" acc=");
        uart::write_hex64(cpu.acc_base);
        uart::writeln("");
        boot_cortex_a53(cpu.acc_base as usize);
        let ack = spin_table.ack_addr(cpu.reg) as usize;
        let mut acknowledged = false;
        for _ in 0..STARTUP_TIMEOUT_US / 10 {
            if read64(ack) == 1 {
                acknowledged = true;
                break;
            }
            delay_us(10);
        }
        if !acknowledged {
            uart::write_str("msm8916: park acknowledgment timeout cpu");
            uart::write_hex64(cpu.reg as u64);
            uart::writeln("");
            return Err(Error::StartupTimeout);
        }
        let entry_el =
            read64(spin_table.release_addr(cpu.reg) as usize + SPIN_TABLE_ENTRY_EL_OFFSET);
        uart::write_str("msm8916: secondary CurrentEL=");
        uart::write_hex64(entry_el);
        uart::writeln("");
        if entry_el != current_el() {
            return Err(Error::EntryState);
        }
        report_secondary_coherency(spin_table, cpu.reg)?;
        uart::writeln("msm8916: secondary parked");
    }

    uart::write_str("msm8916: patched fdt ");
    uart::write_hex64(patched.as_ptr() as u64);
    uart::write_str(" size=");
    uart::write_hex64(patched.len() as u64);
    uart::writeln("");
    Ok(patched.as_ptr() as usize)
}

#[derive(Clone, Copy)]
struct SpinTable {
    addr: u64,
}

impl SpinTable {
    fn code_addr(self) -> u64 {
        self.addr + SPIN_TABLE_ENTRY_OFFSET as u64
    }

    fn release_addr(self, cpu: u32) -> u64 {
        self.addr + SPIN_TABLE_SLOTS_OFFSET as u64 + cpu as u64 * SPIN_TABLE_SLOT_STRIDE as u64
    }

    fn ack_addr(self, cpu: u32) -> u64 {
        self.release_addr(cpu) + SPIN_TABLE_ACK_OFFSET as u64
    }
}

#[derive(Clone, Copy)]
struct CpuInfo {
    reg: u32,
    acc_base: u64,
}

struct CpuList {
    values: [CpuInfo; MAX_CPUS],
    len: usize,
    mode: CpuBootMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuBootMode {
    Cold,
    Resident,
}

impl CpuList {
    fn new() -> Self {
        const EMPTY: CpuInfo = CpuInfo {
            reg: 0,
            acc_base: 0,
        };
        Self {
            values: [EMPTY; MAX_CPUS],
            len: 0,
            mode: CpuBootMode::Cold,
        }
    }

    fn push(&mut self, value: CpuInfo) -> Result<()> {
        if self.len >= self.values.len() {
            return Err(Error::TooManyCpus);
        }
        self.values[self.len] = value;
        self.len += 1;
        Ok(())
    }

    fn as_slice(&self) -> &[CpuInfo] {
        &self.values[..self.len]
    }
}

#[derive(Clone, Copy, Debug)]
enum Error {
    Fdt,
    MissingUart,
    MissingSpinTable,
    BadSpinTable,
    MissingCpu,
    TooManyCpus,
    Scm,
    EntryState,
    OccupiedSpinTable,
    UnparkedResident,
    Coherency,
    StartupTimeout,
}

impl Error {
    fn message(self) -> &'static str {
        match self {
            Self::Fdt => "bad fdt",
            Self::MissingUart => "missing uart",
            Self::MissingSpinTable => "missing spin-table reserved memory",
            Self::BadSpinTable => "bad spin-table reserved memory",
            Self::MissingCpu => "missing cpu data",
            Self::TooManyCpus => "too many cpus",
            Self::Scm => "scm call failed or unsupported convention",
            Self::EntryState => "requires CPU0 at EL1/EL2 with MMU and D-cache off",
            Self::OccupiedSpinTable => {
                "spin-table already occupied; refusing to overwrite resident code"
            }
            Self::UnparkedResident => "resident CPUs have not acknowledged a safe handoff",
            Self::Coherency => "firmware did not enable Cortex-A53 SMPEN coherency",
            Self::StartupTimeout => "secondary did not acknowledge resident parking",
        }
    }
}

type Result<T> = core::result::Result<T, Error>;

impl From<fdt::Error> for Error {
    fn from(_: fdt::Error) -> Self {
        Self::Fdt
    }
}

fn find_uart_base(reader: &Reader<'_>) -> Result<u64> {
    let chosen = reader.find_path(b"/chosen")?;
    let stdout = reader
        .prop_str(chosen, b"stdout-path")
        .or_else(|_| reader.prop_str(chosen, b"linux,stdout-path"))?;
    let target = stdout_target(stdout).ok_or(Error::MissingUart)?;
    let path = if target.starts_with(b"/") {
        target
    } else {
        let aliases = reader.find_path(b"/aliases")?;
        reader.prop_str(aliases, target)?
    };
    let uart = reader.find_path(path)?;
    let compatible = reader.prop(uart, b"compatible")?;
    if !fdt::stringlist_contains(compatible, b"qcom,msm-uartdm-v1.4") {
        return Err(Error::MissingUart);
    }

    let reg = reader.prop(uart, b"reg")?;
    if reg.len() != 8 || fdt::read_cells(reg, 4, 1)? < 0x140 {
        return Err(Error::MissingUart);
    }
    Ok(fdt::read_cells(reg, 0, 1)?)
}

fn stdout_target(stdout: &[u8]) -> Option<&[u8]> {
    let end = stdout
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(stdout.len());
    (end != 0).then_some(&stdout[..end])
}

fn find_spin_table(reader: &Reader<'_>) -> Result<SpinTable> {
    let reserved = reader.find_path(b"/reserved-memory")?;
    if reader.prop_u32(reserved, b"#address-cells")? != 2
        || reader.prop_u32(reserved, b"#size-cells")? != 2
        || !reader.prop(reserved, b"ranges")?.is_empty()
    {
        return Err(Error::BadSpinTable);
    }
    let mut found = None;
    for child in reader.subnodes(reserved)? {
        let child = child?;
        let name = reader.node_name(child)?;
        let compatible = reader.prop(child, b"compatible").unwrap_or(&[]);
        if !fdt::basename_eq(name, b"spin-table")
            && !fdt::stringlist_contains(compatible, SPIN_TABLE_COMPATIBLE)
        {
            continue;
        }
        if found.is_some()
            || !fdt::stringlist_contains(compatible, SPIN_TABLE_COMPATIBLE)
            || !reader.prop(child, b"no-map")?.is_empty()
            || reader.prop(child, b"reusable").is_ok()
            || reader
                .prop_str(child, b"status")
                .is_ok_and(|s| s != b"okay" && s != b"ok")
        {
            return Err(Error::BadSpinTable);
        }
        let reg = reader.prop(child, b"reg")?;
        let addr = fdt::read_cells(reg, 0, 2)?;
        let size = fdt::read_cells(reg, 8, 2)?;
        if reg.len() != 16
            || addr == 0
            || size != SPIN_TABLE_SIZE as u64
            || addr % SPIN_TABLE_SIZE as u64 != 0
            || addr.checked_add(size).is_none_or(|end| end > 1u64 << 32)
        {
            return Err(Error::BadSpinTable);
        }
        found = Some(SpinTable { addr });
    }
    found.ok_or(Error::MissingSpinTable)
}

fn overlaps(start: u64, size: u64, other: u64, other_size: u64) -> bool {
    if size == 0 || other_size == 0 {
        return false;
    }
    start.checked_add(size).is_none_or(|end| {
        other
            .checked_add(other_size)
            .is_none_or(|other_end| start < other_end && other < end)
    })
}

fn validate_memory(
    reader: &Reader<'_>,
    table: SpinTable,
    payload: usize,
    payload_size: usize,
) -> Result<()> {
    let root = reader.root()?;
    if reader.prop_u32(root, b"#address-cells")? != 2
        || reader.prop_u32(root, b"#size-cells")? != 2
        || payload_size == 0
        || overlaps(
            table.addr,
            SPIN_TABLE_SIZE as u64,
            payload as u64,
            payload_size as u64,
        )
    {
        return Err(Error::BadSpinTable);
    }
    // Header/BSS/stack and scratch buffers all precede the aligned payload.
    #[cfg(target_os = "none")]
    {
        let start = crate::_start as *const () as u64;
        if payload as u64 <= start
            || overlaps(
                table.addr,
                SPIN_TABLE_SIZE as u64,
                start,
                payload as u64 - start,
            )
        {
            return Err(Error::BadSpinTable);
        }
    }
    if overlaps(
        table.addr,
        SPIN_TABLE_SIZE as u64,
        reader.data_addr() as u64,
        reader.header().totalsize as u64,
    ) {
        return Err(Error::BadSpinTable);
    }
    for entry in reader.reserve_map()?.chunks_exact(16) {
        let addr = fdt::read_cells(entry, 0, 2)?;
        let size = fdt::read_cells(entry, 8, 2)?;
        if overlaps(table.addr, SPIN_TABLE_SIZE as u64, addr, size)
            && (addr != table.addr || size != SPIN_TABLE_SIZE as u64)
        {
            return Err(Error::BadSpinTable);
        }
    }
    let mut in_ram = false;
    for node in reader.subnodes(root)? {
        let node = node?;
        if reader.prop_str(node, b"device_type").ok() != Some(b"memory") {
            continue;
        }
        let reg = reader.prop(node, b"reg")?;
        if reg.len() % 16 != 0 {
            return Err(Error::BadSpinTable);
        }
        for tuple in reg.chunks_exact(16) {
            let addr = fdt::read_cells(tuple, 0, 2)?;
            let size = fdt::read_cells(tuple, 8, 2)?;
            if addr <= table.addr
                && addr
                    .checked_add(size)
                    .is_some_and(|end| end >= table.addr + SPIN_TABLE_SIZE as u64)
            {
                in_ram = true;
            }
        }
    }
    if !in_ram {
        return Err(Error::BadSpinTable);
    }
    let reserved = reader.find_path(b"/reserved-memory")?;
    for node in reader.subnodes(reserved)? {
        let node = node?;
        if reader
            .prop(node, b"compatible")
            .is_ok_and(|compatible| fdt::stringlist_contains(compatible, SPIN_TABLE_COMPATIBLE))
        {
            continue;
        }
        // Even disabled firmware carveouts are not safe to commandeer.
        let Ok(reg) = reader.prop(node, b"reg") else {
            continue;
        };
        if reg.len() % 16 != 0 {
            return Err(Error::BadSpinTable);
        }
        for tuple in reg.chunks_exact(16) {
            if overlaps(
                table.addr,
                SPIN_TABLE_SIZE as u64,
                fdt::read_cells(tuple, 0, 2)?,
                fdt::read_cells(tuple, 8, 2)?,
            ) {
                return Err(Error::BadSpinTable);
            }
        }
    }
    Ok(())
}

fn collect_cpus(reader: &Reader<'_>) -> Result<CpuList> {
    let cpus_node = reader.find_path(b"/cpus")?;
    if reader.prop_u32(cpus_node, b"#address-cells")? != 1
        || reader.prop_u32(cpus_node, b"#size-cells")? != 0
    {
        return Err(Error::MissingCpu);
    }
    let mut cpus = CpuList::new();

    for child in reader.subnodes(cpus_node)? {
        let child = child?;
        let name = reader.node_name(child)?;
        if !fdt::basename_eq(name, b"cpu") {
            continue;
        }
        if reader.prop_str(child, b"device_type")? != b"cpu" {
            continue;
        }
        let compatible = reader.prop(child, b"compatible")?;
        if !fdt::stringlist_contains(compatible, b"arm,cortex-a53")
            || reader.prop(child, b"reg")?.len() != 4
        {
            return Err(Error::MissingCpu);
        }

        let reg = reader.prop_u32(child, b"reg")?;
        let mode = match reader.prop_str(child, b"enable-method")? {
            b"spin-table" => CpuBootMode::Resident,
            b"psci" | b"pocketboot,msm8916-acc" => CpuBootMode::Cold,
            _ => return Err(Error::MissingCpu),
        };
        if cpus.len != 0 && cpus.mode != mode {
            return Err(Error::OccupiedSpinTable);
        }
        cpus.mode = mode;
        if reg >= MAX_CPUS as u32
            || cpus.as_slice().iter().any(|cpu| cpu.reg == reg)
            || reader
                .prop_str(child, b"status")
                .is_ok_and(|s| s != b"okay" && s != b"ok")
        {
            return Err(Error::MissingCpu);
        }
        let acc_base = if mode == CpuBootMode::Resident {
            let release = reader.prop(child, b"cpu-release-addr")?;
            if release.len() != 8
                || fdt::read_cells(release, 0, 2)? != find_spin_table(reader)?.release_addr(reg)
            {
                return Err(Error::BadSpinTable);
            }
            // A resident handoff needs no cold-start controller phandles in
            // the destination DT. Never touch ACC or SCM in this mode.
            0
        } else {
            let acc_phandle = reader.prop_u32(child, b"qcom,acc")?;
            let acc = reader.find_phandle(acc_phandle)?;
            let acc_reg = reader.prop(acc, b"reg")?;
            let base = fdt::read_cells(acc_reg, 0, 1)?;
            if acc_reg.len() != 8
                || fdt::read_cells(acc_reg, 4, 1)? < 0x18
                || base != 0x0b088000 + reg as u64 * 0x10000
                || !fdt::stringlist_contains(reader.prop(acc, b"compatible")?, b"qcom,msm8916-acc")
            {
                return Err(Error::MissingCpu);
            }
            base
        };

        cpus.push(CpuInfo { reg, acc_base })?;
    }

    if cpus.as_slice().len() != MAX_CPUS {
        return Err(Error::MissingCpu);
    }

    Ok(cpus)
}

fn occupied_prefix(prefix: &[u8]) -> bool {
    prefix.get(0x80..0x88) == Some(b"spin-tab") || prefix.get(0x90..0x96) == Some(b"PBSPIN")
}

fn validate_resident_descriptor(prefix: &[u8]) -> Result<()> {
    // Descriptor integers are little-endian regardless of FDT endianness.
    if prefix.get(..4) != Some(&0x14000040u32.to_le_bytes())
        || prefix.get(0x80..0x88) != Some(b"spin-tab")
        || prefix.get(0x88..0x90) != Some(&[0; 8])
        || prefix.get(0x90..0x98) != Some(b"PBSPIN01")
    {
        return Err(Error::BadSpinTable);
    }
    let fields = [1u32, 4096, 0x100, 0x400, 0x80, 4];
    for (index, value) in fields.iter().enumerate() {
        let offset = 0x98 + index * 4;
        if prefix.get(offset..offset + 4) != Some(&value.to_le_bytes()) {
            return Err(Error::BadSpinTable);
        }
    }
    Ok(())
}

fn has_coherency_diagnostics(prefix: &[u8]) -> bool {
    prefix.get(SPIN_TABLE_DIAG_TAG_OFFSET..SPIN_TABLE_DIAG_TAG_OFFSET + 8)
        == Some(SPIN_TABLE_DIAG_TAG)
}

fn require_coherency(cpuectlr: u64) -> Result<()> {
    if cpuectlr & CPUECTLR_SMPEN != 0 {
        Ok(())
    } else {
        Err(Error::Coherency)
    }
}

fn report_secondary_coherency(table: SpinTable, cpu: u32) -> Result<()> {
    let cpuectlr = read64(table.release_addr(cpu) as usize + SPIN_TABLE_CPUECTLR_OFFSET);
    uart::write_str("msm8916: cpu");
    uart::write_hex64(cpu as u64);
    uart::write_str(" CPUECTLR=");
    uart::write_hex64(cpuectlr);
    uart::writeln("");
    require_coherency(cpuectlr)
}

fn validate_parked_slots(
    table: SpinTable,
    entry_el: u64,
    mut read: impl FnMut(usize) -> u64,
) -> Result<()> {
    if !matches!(entry_el, 4 | 8) || read(table.release_addr(0) as usize) != 0 {
        return Err(Error::UnparkedResident);
    }
    for cpu in 1..MAX_CPUS as u32 {
        let slot = table.release_addr(cpu) as usize;
        let request = read(slot + SPIN_TABLE_REQUEST_OFFSET);
        if request == 0
            || read(slot) != 0
            || read(slot + SPIN_TABLE_ACK_OFFSET) != request
            || read(slot + SPIN_TABLE_ENTRY_EL_OFFSET) != entry_el
            || read(slot + SPIN_TABLE_REQUEST_OFFSET) != request
            || read(slot) != 0
        {
            return Err(Error::UnparkedResident);
        }
    }
    dsb_sy();
    Ok(())
}

fn init_spin_table(spin_table: SpinTable) -> Result<()> {
    let table = spin_table.addr as *mut u8;
    // lk2nd can already have CPUs executing its own spin-table. Even our own
    // descriptor may denote live code from an earlier kernel; never replace it.
    let prefix = unsafe { slice::from_raw_parts(table, 0xb0) };
    if occupied_prefix(prefix) {
        return Err(Error::OccupiedSpinTable);
    }
    let image = resident_image();
    if image.len() < SPIN_TABLE_ENTRY_OFFSET || image.len() > SPIN_TABLE_SLOTS_OFFSET {
        return Err(Error::BadSpinTable);
    }
    unsafe {
        ptr::write_bytes(table, 0, SPIN_TABLE_SIZE);
        ptr::copy_nonoverlapping(image.as_ptr(), table, image.len());
        for cpu in 1..MAX_CPUS as u32 {
            (spin_table.release_addr(cpu) as *mut u64)
                .add(SPIN_TABLE_REQUEST_OFFSET / 8)
                .write_volatile(1u64.to_le());
        }
    }
    clean_dcache_range(spin_table.addr as usize, SPIN_TABLE_SIZE);
    invalidate_icache_range(spin_table.addr as usize, image.len());
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn resident_image() -> &'static [u8] {
    unsafe extern "C" {
        static pocketboot_spin_table_start: u8;
        static pocketboot_spin_table_end: u8;
    }
    unsafe {
        let start = ptr::addr_of!(pocketboot_spin_table_start);
        let end = ptr::addr_of!(pocketboot_spin_table_end);
        slice::from_raw_parts(start, end.offset_from(start) as usize)
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn resident_image() -> &'static [u8] {
    &[]
}

fn boot_cortex_a53(acc_base: usize) {
    const APCS_CPU_PWR_CTL: usize = 0x04;
    const CORE_PWRD_UP: u32 = 1 << 7;
    const COREPOR_RST: u32 = 1 << 5;
    const CORE_RST: u32 = 1 << 4;
    const CORE_MEM_HS: u32 = 1 << 3;
    const CORE_MEM_CLAMP: u32 = 1 << 1;
    const CLAMP: u32 = 1 << 0;

    const APC_PWR_GATE_CTL: usize = 0x14;
    const GDHS_CNT_SHIFT: u32 = 24;
    const GDHS_EN: u32 = 1 << 0;

    let mut reg = CORE_RST | COREPOR_RST | CLAMP | CORE_MEM_CLAMP;
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();

    write32(
        acc_base + APC_PWR_GATE_CTL,
        GDHS_EN | (0x10 << GDHS_CNT_SHIFT),
    );
    dsb_sy();
    delay_us(2);

    reg &= !CORE_MEM_CLAMP;
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();

    reg |= CORE_MEM_HS;
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();
    delay_us(2);

    reg &= !CLAMP;
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();
    delay_us(2);

    reg &= !(CORE_RST | COREPOR_RST);
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();

    reg |= CORE_PWRD_UP;
    write32(acc_base + APCS_CPU_PWR_CTL, reg);
    dsb_sy();
}

fn is_psci_node(reader: &Reader<'_>, node: Node) -> bool {
    reader.prop(node, b"compatible").is_ok_and(|compatible| {
        [b"arm,psci".as_slice(), b"arm,psci-0.2", b"arm,psci-1.0"]
            .iter()
            .any(|name| fdt::stringlist_contains(compatible, name))
    })
}

#[derive(Clone, Copy)]
struct PowerProvider {
    phandle: u32,
    cells: usize,
    psci: bool,
}
struct PowerProviders {
    entries: [PowerProvider; 64],
    len: usize,
}

fn power_domain_providers(reader: &Reader<'_>) -> Result<PowerProviders> {
    fn visit(
        reader: &Reader<'_>,
        node: Node,
        parent_psci: bool,
        depth: usize,
        out: &mut PowerProviders,
    ) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(Error::Fdt);
        }
        let psci = parent_psci || is_psci_node(reader, node);
        if let Ok(cells) = reader.prop(node, b"#power-domain-cells") {
            if cells.len() != 4 {
                return Err(Error::Fdt);
            }
            if let Ok(phandle) = reader.node_phandle(node) {
                if phandle == 0
                    || phandle == u32::MAX
                    || out.len == out.entries.len()
                    || out.entries[..out.len]
                        .iter()
                        .any(|provider| provider.phandle == phandle)
                {
                    return Err(Error::Fdt);
                }
                out.entries[out.len] = PowerProvider {
                    phandle,
                    cells: fdt::read_be32(cells, 0)? as usize,
                    psci,
                };
                out.len += 1;
            }
        }
        for child in reader.subnodes(node)? {
            visit(reader, child?, psci, depth + 1, out)?;
        }
        Ok(())
    }
    let mut out = PowerProviders {
        entries: [PowerProvider {
            phandle: 0,
            cells: 0,
            psci: false,
        }; 64],
        len: 0,
    };
    visit(reader, reader.root()?, false, 0, &mut out)?;
    Ok(out)
}

struct FilteredDomains {
    domains: [u8; 256],
    domains_len: usize,
    names: [u8; 512],
    names_len: usize,
}

fn filtered_power_domains(
    reader: &Reader<'_>,
    node: Node,
    providers: &PowerProviders,
) -> Result<FilteredDomains> {
    let mut out = FilteredDomains {
        domains: [0; 256],
        domains_len: 0,
        names: [0; 512],
        names_len: 0,
    };
    let names = reader.prop(node, b"power-domain-names").ok();
    let Ok(domains) = reader.prop(node, b"power-domains") else {
        return if names.is_some() {
            Err(Error::Fdt)
        } else {
            Ok(out)
        };
    };
    if domains.len() % 4 != 0 {
        return Err(Error::Fdt);
    }
    let mut offset = 0;
    let mut name_offset = 0;
    while offset < domains.len() {
        let phandle = fdt::read_be32(domains, offset)?;
        let provider = providers.entries[..providers.len]
            .iter()
            .find(|provider| provider.phandle == phandle)
            .ok_or(Error::Fdt)?;
        let bytes = provider
            .cells
            .checked_add(1)
            .and_then(|cells| cells.checked_mul(4))
            .ok_or(Error::Fdt)?;
        let end = offset.checked_add(bytes).ok_or(Error::Fdt)?;
        if end > domains.len() {
            return Err(Error::Fdt);
        }
        let name_end = match names {
            Some(names) => fdt::find_nul(names, name_offset)?
                .checked_add(1)
                .ok_or(Error::Fdt)?,
            None => 0,
        };
        if !provider.psci {
            let out_end = out.domains_len.checked_add(bytes).ok_or(Error::Fdt)?;
            if out_end > out.domains.len() {
                return Err(Error::Fdt);
            }
            out.domains[out.domains_len..out_end].copy_from_slice(&domains[offset..end]);
            out.domains_len = out_end;
            if let Some(names) = names {
                let out_end = out
                    .names_len
                    .checked_add(name_end - name_offset)
                    .ok_or(Error::Fdt)?;
                if out_end > out.names.len() {
                    return Err(Error::Fdt);
                }
                out.names[out.names_len..out_end].copy_from_slice(&names[name_offset..name_end]);
                out.names_len = out_end;
            }
        }
        offset = end;
        name_offset = name_end;
    }
    if names.is_some_and(|names| name_offset != names.len()) {
        return Err(Error::Fdt);
    }
    Ok(out)
}

const PATCH_NAMES: [&[u8]; 5] = [
    b"enable-method",
    b"cpu-release-addr",
    b"status",
    b"power-domains",
    b"power-domain-names",
];
const NAME_ENABLE_METHOD: usize = 0;
const NAME_CPU_RELEASE_ADDR: usize = 1;
const NAME_STATUS: usize = 2;
const NAME_POWER_DOMAINS: usize = 3;
const NAME_POWER_DOMAIN_NAMES: usize = 4;

struct PatchStrings {
    offsets: [u32; PATCH_NAMES.len()],
    append: [bool; PATCH_NAMES.len()],
    size: usize,
}

impl PatchStrings {
    fn new(strings: &[u8]) -> Result<Self> {
        let mut offsets = [0u32; PATCH_NAMES.len()];
        let mut append = [false; PATCH_NAMES.len()];
        let mut size = strings.len();

        for (index, name) in PATCH_NAMES.iter().enumerate() {
            if let Some(offset) = fdt::find_string(strings, name) {
                offsets[index] = offset;
            } else {
                offsets[index] = fdt::u32_len(size)?;
                append[index] = true;
                size = size.checked_add(name.len() + 1).ok_or(Error::Fdt)?;
            }
        }

        Ok(Self {
            offsets,
            append,
            size,
        })
    }

    fn offset(&self, index: usize) -> u32 {
        self.offsets[index]
    }

    fn write(&self, writer: &mut Writer<'_>, old_strings: &[u8]) -> Result<()> {
        writer.write(old_strings)?;
        for (index, name) in PATCH_NAMES.iter().enumerate() {
            if self.append[index] {
                writer.write(name)?;
                writer.write(&[0])?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Root,
    Cpus,
    Cpu,
    IdleStates,
    IdleState,
    Other,
}

#[derive(Clone, Copy)]
struct PatchContext {
    kind: NodeKind,
    disable: bool,
}

impl PatchContext {
    const OTHER: Self = Self {
        kind: NodeKind::Other,
        disable: false,
    };
}

fn patch_fdt<'a>(
    reader: &Reader<'_>,
    output: &'a mut [u8],
    spin_table: SpinTable,
) -> Result<&'a mut [u8]> {
    let header = reader.header();
    let reserve_map = reader.reserve_map()?;
    let structure = reader.struct_block()?;
    let strings = reader.strings()?;
    let patch_strings = PatchStrings::new(strings)?;
    let providers = power_domain_providers(reader)?;

    let mut writer = Writer::new(output);
    writer.reserve(40)?;
    writer.write(reserve_map)?;
    writer.pad_to(4)?;

    let off_dt_struct = writer.pos();
    patch_structure(
        reader,
        structure,
        strings,
        &patch_strings,
        &mut writer,
        spin_table,
        &providers,
    )?;
    let size_dt_struct = writer.pos() - off_dt_struct;

    let off_dt_strings = writer.pos();
    patch_strings.write(&mut writer, strings)?;
    let size_dt_strings = writer.pos() - off_dt_strings;
    if size_dt_strings != patch_strings.size {
        return Err(Error::Fdt);
    }
    let totalsize = writer.pos();

    fdt::write_header(
        &mut writer,
        header,
        totalsize,
        off_dt_struct,
        off_dt_strings,
        size_dt_struct,
        size_dt_strings,
    )?;

    Ok(writer.finish())
}

fn patch_structure(
    reader: &Reader<'_>,
    structure: &[u8],
    strings: &[u8],
    patch_strings: &PatchStrings,
    writer: &mut Writer<'_>,
    spin_table: SpinTable,
    providers: &PowerProviders,
) -> Result<()> {
    let mut stack = [PatchContext::OTHER; MAX_DEPTH];
    let mut cursor = 0usize;
    let mut depth = 0usize;

    loop {
        let token_start = cursor;
        let token = fdt::read_be32(structure, cursor)?;
        cursor = cursor.checked_add(4).ok_or(Error::Fdt)?;

        match token {
            FDT_BEGIN_NODE => {
                let name_start = cursor;
                let name_end = fdt::find_nul(structure, name_start)?;
                let name = &structure[name_start..name_end];
                let next = fdt::align_usize(name_end.checked_add(1).ok_or(Error::Fdt)?, 4)?;
                if depth >= stack.len() {
                    return Err(Error::Fdt);
                }

                let parent = if depth == 0 {
                    PatchContext::OTHER
                } else {
                    stack[depth - 1]
                };
                let node = Node {
                    offset: token_start,
                };
                let kind = node_kind(depth, parent.kind, name);
                let disable = parent.disable
                    || is_psci_node(reader, node)
                    || matches!(kind, NodeKind::IdleState);
                writer.write(&structure[token_start..next])?;
                if kind == NodeKind::Cpu {
                    let domains = filtered_power_domains(reader, node, providers)?;
                    if domains.domains_len != 0 {
                        fdt::write_prop(
                            writer,
                            patch_strings.offset(NAME_POWER_DOMAINS),
                            &domains.domains[..domains.domains_len],
                        )?;
                        if domains.names_len != 0 {
                            fdt::write_prop(
                                writer,
                                patch_strings.offset(NAME_POWER_DOMAIN_NAMES),
                                &domains.names[..domains.names_len],
                            )?;
                        }
                    }
                    fdt::write_prop(
                        writer,
                        patch_strings.offset(NAME_ENABLE_METHOD),
                        b"spin-table\0",
                    )?;
                    fdt::write_prop(
                        writer,
                        patch_strings.offset(NAME_CPU_RELEASE_ADDR),
                        &spin_table
                            .release_addr(reader.prop_u32(node, b"reg")?)
                            .to_be_bytes(),
                    )?;
                }
                if disable {
                    fdt::write_prop(writer, patch_strings.offset(NAME_STATUS), b"disabled\0")?;
                }

                stack[depth] = PatchContext { kind, disable };
                depth += 1;
                cursor = next;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return Err(Error::Fdt);
                }
                writer.write(&structure[token_start..cursor])?;
                depth -= 1;
            }
            FDT_PROP => {
                let parts = fdt::property_parts(structure, cursor)?;
                let name = fdt::string_at(strings, parts.nameoff).ok_or(Error::Fdt)?;
                let context = if depth == 0 {
                    PatchContext::OTHER
                } else {
                    stack[depth - 1]
                };
                if !should_skip_prop(context, name) {
                    writer.write(&structure[token_start..parts.next])?;
                }
                cursor = parts.next;
            }
            FDT_NOP => writer.write(&structure[token_start..cursor])?,
            FDT_END => {
                if depth != 0 {
                    return Err(Error::Fdt);
                }
                writer.write(&structure[token_start..cursor])?;
                return Ok(());
            }
            _ => return Err(Error::Fdt),
        }
    }
}

fn node_kind(depth: usize, parent: NodeKind, name: &[u8]) -> NodeKind {
    if depth == 0 {
        if name.is_empty() {
            return NodeKind::Root;
        }
        return NodeKind::Other;
    }

    match parent {
        NodeKind::Root if name == b"cpus" => NodeKind::Cpus,
        NodeKind::Cpus if fdt::basename_eq(name, b"cpu") => NodeKind::Cpu,
        NodeKind::Cpus if name == b"idle-states" => NodeKind::IdleStates,
        NodeKind::IdleStates => NodeKind::IdleState,
        _ => NodeKind::Other,
    }
}

fn should_skip_prop(context: PatchContext, name: &[u8]) -> bool {
    if context.disable && name == b"status" {
        return true;
    }
    match context.kind {
        NodeKind::Cpu => matches!(
            name,
            b"enable-method"
                | b"cpu-release-addr"
                | b"power-domains"
                | b"power-domain-names"
                | b"cpu-idle-states"
        ),
        NodeKind::IdleStates => name == b"entry-method",
        _ => false,
    }
}

mod scm {
    use super::*;

    pub(super) const BOOT_MC_FLAG_AARCH64: u32 = 1 << 0;
    pub(super) const BOOT_MC_FLAG_COLDBOOT: u32 = 1 << 1;
    const SVC_BOOT: u32 = 0x01;
    const BOOT_SET_ADDR_MC: u32 = 0x11;
    const SVC_INFO: u32 = 0x06;
    const INFO_IS_CALL_AVAIL: u32 = 0x01;
    const OWNER_SIP: u32 = 2;
    const SCM_INTERRUPTED: i32 = 1;
    const SCM_EBUSY: i32 = -12;
    const MAX_ARGS: usize = 10;
    const N_EXT_ARGS: usize = 7;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Convention {
        Smc32,
        Smc64,
    }

    impl Convention {
        fn function(self, svc: u32, cmd: u32, fast: bool) -> u32 {
            ((fast as u32) << 31)
                | ((self == Self::Smc64) as u32) << 30
                | (OWNER_SIP << 24)
                | ((svc & 0xff) << 8)
                | (cmd & 0xff)
        }
    }

    #[repr(align(64))]
    struct ExtArgs([u8; N_EXT_ARGS * 8]);
    static mut EXT_ARGS: ExtArgs = ExtArgs([0; N_EXT_ARGS * 8]);

    pub(super) fn set_boot_addr_mc(entry: u64, flags: u32) -> Result<()> {
        // A 64-bit client can still be running over 32-bit Qualcomm SCM.
        // Probe INFO_IS_CALL_AVAIL itself using fast calls, as Linux does.
        let convention = probe_convention()?;
        uart::writeln(match convention {
            Convention::Smc32 => "msm8916: SCM SMC32",
            Convention::Smc64 => "msm8916: SCM SMC64",
        });
        if !is_call_available(convention, SVC_BOOT, BOOT_SET_ADDR_MC, false)? {
            return Err(Error::Scm);
        }
        if convention == Convention::Smc32 && entry > u32::MAX as u64 {
            return Err(Error::Scm);
        }
        let mut args = [0u64; MAX_ARGS];
        args[0] = entry;
        args[1..5].fill(u64::MAX);
        args[5] = flags as u64;
        call(convention, SVC_BOOT, BOOT_SET_ADDR_MC, 6, &args, false).map(|_| ())
    }

    fn probe_convention() -> Result<Convention> {
        for convention in [Convention::Smc64, Convention::Smc32] {
            if matches!(
                is_call_available(convention, SVC_INFO, INFO_IS_CALL_AVAIL, true),
                Ok(true)
            ) {
                return Ok(convention);
            }
        }
        // The legacy SCM convention has no SET_BOOT_ADDR_MC service. Do not
        // turn a failed probe into a speculative legacy or PSCI CPU_ON call.
        Err(Error::Scm)
    }

    fn is_call_available(convention: Convention, svc: u32, cmd: u32, fast: bool) -> Result<bool> {
        let mut args = [0u64; MAX_ARGS];
        args[0] = ((OWNER_SIP << 24) | ((svc & 0xff) << 8) | (cmd & 0xff)) as u64;
        call(convention, SVC_INFO, INFO_IS_CALL_AVAIL, 1, &args, fast).map(|res| res.a1 == 1)
    }

    fn pack_ext(convention: Convention, args: &[u64; MAX_ARGS], output: &mut [u8; N_EXT_ARGS * 8]) {
        output.fill(0);
        for (index, value) in args[3..].iter().enumerate() {
            match convention {
                Convention::Smc32 => {
                    output[index * 4..index * 4 + 4].copy_from_slice(&(*value as u32).to_le_bytes())
                }
                Convention::Smc64 => {
                    output[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes())
                }
            }
        }
    }

    fn call(
        convention: Convention,
        svc: u32,
        cmd: u32,
        arglen: u32,
        args: &[u64; MAX_ARGS],
        fast: bool,
    ) -> Result<SmcccRes> {
        if arglen > MAX_ARGS as u32 {
            return Err(Error::Scm);
        }
        let mut smc_args = [0u64; 8];
        smc_args[0] = convention.function(svc, cmd, fast) as u64;
        smc_args[1] = arglen as u64; // All SET_BOOT_ADDR_MC arguments are values.
        smc_args[2..6].copy_from_slice(&args[..4]);
        if arglen > 4 {
            unsafe {
                let ext = ptr::addr_of_mut!(EXT_ARGS);
                pack_ext(convention, args, &mut (*ext).0);
                smc_args[5] = ptr::addr_of!((*ext).0) as u64;
                if convention == Convention::Smc32 && smc_args[5] > u32::MAX as u64 {
                    return Err(Error::Scm);
                }
                clean_dcache_range(smc_args[5] as usize, N_EXT_ARGS * 8);
            }
        }
        if convention == Convention::Smc32 {
            for value in &mut smc_args[2..6] {
                *value = *value as u32 as u64;
            }
        }
        for attempt in 0..=20 {
            if attempt == 0 {
                uart::write_str("msm8916: SMC enter function=");
                uart::write_hex64(smc_args[0]);
                uart::writeln("");
            }
            let res = smc_do(smc_args)?;
            uart::write_str("msm8916: SMC returned status=");
            uart::write_hex64(res.a0);
            uart::writeln("");
            // SMC32 returns may be zero-extended; compare the signed low word.
            let status = res.a0 as u32 as i32;
            if status == SCM_EBUSY && attempt < 20 {
                delay_us(30_000);
                continue;
            }
            if status != 0 {
                uart::write_str("msm8916: SCM function=");
                uart::write_hex64(smc_args[0]);
                uart::write_str(" status=");
                uart::write_hex64(res.a0);
                uart::writeln("");
                return Err(Error::Scm);
            }
            return Ok(res);
        }
        Err(Error::Scm)
    }

    struct SmcccRes {
        a0: u64,
        a1: u64,
    }

    #[cfg(target_arch = "aarch64")]
    fn smc_do(args: [u64; 8]) -> Result<SmcccRes> {
        let mut call_x0 = args[0];
        let mut cookie = 0u64;
        // Qualcomm's interrupted-call protocol preserves x6 between retries,
        // but reloads the original x1..x5 arguments. Declare all volatile
        // registers clobbered, rather than promising x2..x5 survive firmware.
        for _ in 0..1024 {
            let mut x0 = call_x0;
            let mut x1 = args[1];
            unsafe {
                core::arch::asm!(
                    "smc #0",
                    inout("x0") x0,
                    inout("x1") x1,
                    inout("x2") args[2] => _,
                    inout("x3") args[3] => _,
                    inout("x4") args[4] => _,
                    inout("x5") args[5] => _,
                    inout("x6") cookie,
                    inout("x7") args[7] => _,
                    clobber_abi("C"),
                    options(nostack)
                );
            }
            if x0 as u32 as i32 != SCM_INTERRUPTED {
                return Ok(SmcccRes { a0: x0, a1: x1 });
            }
            call_x0 = SCM_INTERRUPTED as u64;
        }
        Err(Error::Scm)
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn smc_do(_args: [u64; 8]) -> Result<SmcccRes> {
        Err(Error::Scm)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn scm_convention_and_extended_argument_encoding() {
            assert_eq!(Convention::Smc64.function(6, 1, true), 0xc2000601);
            assert_eq!(Convention::Smc32.function(6, 1, true), 0x82000601);
            assert_eq!(Convention::Smc64.function(1, 0x11, false), 0x42000111);
            let mut args = [0; MAX_ARGS];
            args[3] = 0x123456789abcdef0;
            args[4] = u64::MAX;
            args[5] = 3;
            let mut ext = [0; N_EXT_ARGS * 8];
            pack_ext(Convention::Smc32, &args, &mut ext);
            assert_eq!(
                &ext[..12],
                &[0xf0, 0xde, 0xbc, 0x9a, 0xff, 0xff, 0xff, 0xff, 3, 0, 0, 0]
            );
            assert_eq!(&ext[28..], &[0; 28]);
            pack_ext(Convention::Smc64, &args, &mut ext);
            assert_eq!(&ext[..8], &args[3].to_le_bytes());
            assert_eq!(&ext[16..24], &3u64.to_le_bytes());
        }
    }
}

fn read64(address: usize) -> u64 {
    u64::from_le(unsafe { (address as *const u64).read_volatile() })
}

#[cfg(target_arch = "aarch64")]
fn read_cpuectlr() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, S3_1_C15_C2_1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

#[cfg(not(target_arch = "aarch64"))]
fn read_cpuectlr() -> u64 {
    CPUECTLR_SMPEN
}

#[cfg(target_arch = "aarch64")]
fn current_el() -> u64 {
    let el: u64;
    unsafe {
        core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack, preserves_flags));
    }
    el
}

#[cfg(not(target_arch = "aarch64"))]
fn current_el() -> u64 {
    4
}

#[cfg(target_arch = "aarch64")]
fn cache_mmu_off() -> bool {
    let el = current_el();
    let sctlr: u64;
    unsafe {
        match el {
            4 => {
                core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack, preserves_flags))
            }
            8 => {
                core::arch::asm!("mrs {}, sctlr_el2", out(reg) sctlr, options(nomem, nostack, preserves_flags))
            }
            _ => return false,
        }
    }
    sctlr & ((1 << 2) | 1) == 0
}

#[cfg(not(target_arch = "aarch64"))]
fn cache_mmu_off() -> bool {
    true
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

fn write32(address: usize, value: u32) {
    unsafe {
        (address as *mut u32).write_volatile(value);
    }
}

#[cfg(target_arch = "aarch64")]
fn read_mpidr() -> u32 {
    let value: u64;
    unsafe {
        core::arch::asm!("mrs {}, mpidr_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    ((value & 0xffffff) | ((value >> 8) & 0xff000000)) as u32
}

#[cfg(not(target_arch = "aarch64"))]
fn read_mpidr() -> u32 {
    0
}

#[cfg(target_arch = "aarch64")]
fn dsb_sy() {
    unsafe {
        core::arch::asm!("dsb sy", options(nomem, nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn dsb_sy() {}

#[cfg(target_arch = "aarch64")]
fn timer_frequency() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack, preserves_flags));
    }
    freq
}

#[cfg(not(target_arch = "aarch64"))]
fn timer_frequency() -> u64 {
    19_200_000
}

#[cfg(target_arch = "aarch64")]
fn delay_us(us: u64) {
    let freq = timer_frequency();
    let start: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) start, options(nomem, nostack, preserves_flags));
    }
    if freq == 0 {
        return;
    }
    let ticks = freq.saturating_mul(us).div_ceil(1_000_000);
    loop {
        let now: u64;
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) now, options(nomem, nostack, preserves_flags));
        }
        if now.wrapping_sub(start) >= ticks {
            break;
        }
        core::hint::spin_loop();
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn delay_us(_us: u64) {}

#[cfg(target_arch = "aarch64")]
fn clean_dcache_range(address: usize, len: usize) {
    let line = dcache_line_size();
    let end = address.saturating_add(len);
    let mut current = address & !(line - 1);
    while current < end {
        unsafe {
            core::arch::asm!("dc cvac, {}", in(reg) current, options(nostack, preserves_flags));
        }
        current += line;
    }
    dsb_sy();
}

#[cfg(not(target_arch = "aarch64"))]
fn clean_dcache_range(_address: usize, _len: usize) {}

#[cfg(target_arch = "aarch64")]
fn invalidate_icache_range(address: usize, len: usize) {
    let line = icache_line_size();
    let end = address.saturating_add(len);
    let mut current = address & !(line - 1);
    while current < end {
        unsafe {
            core::arch::asm!("ic ivau, {}", in(reg) current, options(nostack, preserves_flags));
        }
        current += line;
    }
    dsb_sy();
    unsafe {
        core::arch::asm!("isb", options(nomem, nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn invalidate_icache_range(_address: usize, _len: usize) {}

#[cfg(target_arch = "aarch64")]
fn dcache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
    }
    4usize << ((ctr >> 16) & 0xf)
}

#[cfg(target_arch = "aarch64")]
fn icache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
    }
    4usize << (ctr & 0xf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fdt::tests::{
        ensure_string_vec, test_dtb, write_be32_vec, write_be64_vec, write_begin_node_vec,
        write_prop_vec,
    };

    #[test]
    fn discovers_uart_from_stdout_alias() {
        let dtb = msm8916_test_dtb(false);
        let reader = Reader::new(&dtb).unwrap();
        assert_eq!(find_uart_base(&reader).unwrap(), 0x078b0000);
    }

    #[test]
    fn requires_explicit_spin_table_reserved_memory() {
        let dtb = msm8916_test_dtb(false);
        let reader = Reader::new(&dtb).unwrap();
        assert!(matches!(
            find_spin_table(&reader),
            Err(Error::MissingSpinTable)
        ));
    }

    #[test]
    fn patches_cpu_nodes_for_spin_table() {
        let dtb = msm8916_test_dtb(true);
        let reader = Reader::new(&dtb).unwrap();
        let mut output = vec![0; 16 * 1024];
        let patched = patch_fdt(&reader, &mut output, SpinTable { addr: 0x9000_0000 }).unwrap();
        let patched = Reader::new(patched).unwrap();
        let cpu0 = patched.find_path(b"/cpus/cpu@0").unwrap();
        assert_eq!(
            patched.prop(cpu0, b"cpu-release-addr").unwrap(),
            0x90000400u64.to_be_bytes()
        );
        let cpu1 = patched.find_path(b"/cpus/cpu@1").unwrap();

        assert_eq!(
            patched.prop_str(cpu1, b"enable-method").unwrap(),
            b"spin-table"
        );
        assert_eq!(
            patched.prop(cpu1, b"cpu-release-addr").unwrap(),
            0x9000_0480u64.to_be_bytes()
        );
        assert!(patched.prop(cpu1, b"power-domains").is_err());
        assert!(patched.prop(cpu1, b"cpu-idle-states").is_err());
        assert!(patched.prop(cpu1, b"power-domain-names").is_err());

        let psci = patched.find_path(b"/firmware-cpu-service").unwrap();
        assert_eq!(patched.prop_str(psci, b"status").unwrap(), b"disabled");

        let saw_node = patched.find_path(b"/soc@0/power-manager@b099000").unwrap();
        assert_eq!(patched.prop_str(saw_node, b"status").unwrap(), b"reserved");

        let idle = patched.find_path(b"/cpus/idle-states/cpu-sleep-0").unwrap();
        assert_eq!(patched.prop_str(idle, b"status").unwrap(), b"disabled");
        assert_eq!(
            patched.prop_str(idle, b"compatible").unwrap(),
            b"arm,idle-state"
        );
    }

    fn replace_prop(dtb: &mut [u8], path: &[u8], name: &[u8], replacement: &[u8]) {
        let reader = Reader::new(dtb).unwrap();
        let node = reader.find_path(path).unwrap();
        let value = reader.prop(node, name).unwrap();
        let offset = value.as_ptr() as usize - dtb.as_ptr() as usize;
        assert_eq!(value.len(), replacement.len());
        dtb[offset..offset + replacement.len()].copy_from_slice(replacement);
    }

    #[test]
    fn validates_four_cpu_topology_and_reserved_memory() {
        let dtb = msm8916_test_dtb(true);
        let reader = Reader::new(&dtb).unwrap();
        let table = find_spin_table(&reader).unwrap();
        assert_eq!(collect_cpus(&reader).unwrap().as_slice().len(), 4);
        assert_eq!(table.code_addr(), 0x90000100);
        assert_eq!(table.release_addr(3), 0x90000580);
        assert_eq!(table.ack_addr(3), 0x900005c0);
        validate_memory(&reader, table, 0x80200000, 0x1000000).unwrap();
        assert!(validate_memory(&reader, table, 0x90000000, 4096).is_err());
        assert!(validate_memory(&reader, table, 0x80200000, 0).is_err());
        assert!(!overlaps(0x90000000, 4096, 0x90000010, 0));
    }

    #[test]
    fn rejects_duplicate_affinity_and_incorrect_acc() {
        for reg in [0u32, 0x101, 4] {
            let mut dtb = msm8916_test_dtb(true);
            replace_prop(&mut dtb, b"/cpus/cpu@1", b"reg", &reg.to_be_bytes());
            assert!(collect_cpus(&Reader::new(&dtb).unwrap()).is_err());
        }
        let mut dtb = msm8916_test_dtb(true);
        replace_prop(
            &mut dtb,
            b"/cpus/cpu@1",
            b"qcom,acc",
            &0x100u32.to_be_bytes(),
        );
        assert!(collect_cpus(&Reader::new(&dtb).unwrap()).is_err());
    }

    #[test]
    fn rejects_unknown_or_wrong_sized_resident_abi() {
        let path = b"/reserved-memory/spin-table@90000000";
        let mut dtb = msm8916_test_dtb(true);
        replace_prop(&mut dtb, path, b"compatible", b"pocketboot,spin-table-v2\0");
        assert!(find_spin_table(&Reader::new(&dtb).unwrap()).is_err());
        for (addr, size) in [
            (0x90000008u64, 4096u64),
            (0x90000000, 144),
            (0x100000000, 4096),
        ] {
            let mut dtb = msm8916_test_dtb(true);
            let mut reg = [0; 16];
            reg[..8].copy_from_slice(&addr.to_be_bytes());
            reg[8..].copy_from_slice(&size.to_be_bytes());
            replace_prop(&mut dtb, path, b"reg", &reg);
            assert!(find_spin_table(&Reader::new(&dtb).unwrap()).is_err());
        }
        let mut prefix = [0; 0xb0];
        assert!(!occupied_prefix(&prefix));
        prefix[0x80..0x88].copy_from_slice(b"spin-tab");
        assert!(occupied_prefix(&prefix));
        prefix.fill(0);
        prefix[0x90..0x98].copy_from_slice(b"PBSPIN02");
        assert!(occupied_prefix(&prefix));
    }

    #[test]
    fn patched_tree_requires_resident_handoff() {
        let dtb = msm8916_test_dtb(true);
        let reader = Reader::new(&dtb).unwrap();
        let table = find_spin_table(&reader).unwrap();
        assert!(patch_fdt(&reader, &mut [0; 40], table).is_err());
        let mut output = vec![0; 16384];
        let patched = patch_fdt(&reader, &mut output, table).unwrap();
        let reader = Reader::new(patched).unwrap();
        let cpus = collect_cpus(&reader).unwrap();
        assert_eq!(cpus.mode, CpuBootMode::Resident);
        assert!(cpus.as_slice().iter().all(|cpu| cpu.acc_base == 0));
        for cpu in 0..4u32 {
            let node = reader
                .find_path(format!("/cpus/cpu@{cpu:x}").as_bytes())
                .unwrap();
            assert_eq!(
                reader.prop(node, b"cpu-release-addr").unwrap(),
                table.release_addr(cpu).to_be_bytes()
            );
        }
    }

    #[test]
    fn resident_handoff_requires_exact_descriptor_and_parked_generations() {
        let mut prefix = [0; 0xb0];
        prefix[..4].copy_from_slice(&0x14000040u32.to_le_bytes());
        prefix[0x80..0x88].copy_from_slice(b"spin-tab");
        prefix[0x90..0x98].copy_from_slice(b"PBSPIN01");
        for (index, value) in [1u32, 4096, 0x100, 0x400, 0x80, 4].iter().enumerate() {
            prefix[0x98 + index * 4..0x9c + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        validate_resident_descriptor(&prefix).unwrap();
        for offset in [0, 0x80, 0x88, 0x90, 0x98, 0x9c, 0xa0, 0xa4, 0xa8, 0xac] {
            let mut bad = prefix;
            bad[offset] ^= 1;
            assert!(
                validate_resident_descriptor(&bad).is_err(),
                "offset {offset:x}"
            );
        }
        let table = SpinTable { addr: 0 };
        let mut words = [0u64; SPIN_TABLE_SIZE / 8];
        for cpu in 1..4 {
            let slot = table.release_addr(cpu) as usize / 8;
            words[slot + SPIN_TABLE_REQUEST_OFFSET / 8] = cpu as u64 + 2;
            words[slot + SPIN_TABLE_ACK_OFFSET / 8] = cpu as u64 + 2;
            words[slot + SPIN_TABLE_ENTRY_EL_OFFSET / 8] = 4;
        }
        validate_parked_slots(table, 4, |addr| words[addr / 8]).unwrap();
        assert!(validate_parked_slots(table, 8, |addr| words[addr / 8]).is_err());
        for cpu in 1..4 {
            for (offset, value) in [(0, 0x80200000), (8, 0), (0x40, 1), (0x48, 8)] {
                let mut bad = words;
                bad[(table.release_addr(cpu) as usize + offset) / 8] = value;
                assert!(validate_parked_slots(table, 4, |addr| bad[addr / 8]).is_err());
            }
        }
        words[table.release_addr(0) as usize / 8] = 0x80200000;
        assert!(validate_parked_slots(table, 4, |addr| words[addr / 8]).is_err());
    }

    #[test]
    fn coherency_snapshots_require_explicit_diagnostic_tag() {
        let mut prefix = [0; 0xb8];
        assert!(!has_coherency_diagnostics(&prefix));
        assert!(!has_coherency_diagnostics(&prefix[..0xb0]));
        prefix[0xb0..0xb8].copy_from_slice(b"PBSDIAG2");
        assert!(!has_coherency_diagnostics(&prefix));
        prefix[0xb0..0xb8].copy_from_slice(SPIN_TABLE_DIAG_TAG);
        assert!(has_coherency_diagnostics(&prefix));
        for value in [0, 1, 0x3f, u64::MAX & !CPUECTLR_SMPEN] {
            assert!(require_coherency(value).is_err());
        }
        for value in [0x40, 0x47, u64::MAX] {
            require_coherency(value).unwrap();
        }
    }

    #[test]
    fn resident_handoff_rejects_mixed_methods_and_wrong_release_slots() {
        let dtb = msm8916_test_dtb(true);
        let reader = Reader::new(&dtb).unwrap();
        let table = find_spin_table(&reader).unwrap();
        let mut output = vec![0; 16384];
        let patched = patch_fdt(&reader, &mut output, table).unwrap().to_vec();
        for cpu in 0..4 {
            let path = format!("/cpus/cpu@{cpu:x}");
            let mut bad = patched.clone();
            replace_prop(
                &mut bad,
                path.as_bytes(),
                b"cpu-release-addr",
                &table.release_addr((cpu + 1) % 4).to_be_bytes(),
            );
            assert!(collect_cpus(&Reader::new(&bad).unwrap()).is_err());
            let mut bad = patched.clone();
            replace_prop(
                &mut bad,
                path.as_bytes(),
                b"enable-method",
                b"psci\0\0\0\0\0\0\0",
            );
            assert!(collect_cpus(&Reader::new(&bad).unwrap()).is_err());
        }
    }

    #[test]
    fn rejects_overlapping_firmware_memreserve() {
        for (address, size, allowed) in [
            (0x8ffff000u64, 0x2000u64, false),
            (0x90000000, 0x1000, true),
        ] {
            let mut dtb = msm8916_test_dtb(true);
            let reserve_offset = Reader::new(&dtb).unwrap().header().off_mem_rsvmap;
            let entry = [address.to_be_bytes(), size.to_be_bytes()].concat();
            dtb.splice(reserve_offset..reserve_offset, entry);
            for offset in [4, 8, 12] {
                let old = fdt::read_be32(&dtb, offset).unwrap();
                dtb[offset..offset + 4].copy_from_slice(&(old + 16).to_be_bytes());
            }
            let reader = Reader::new(&dtb).unwrap();
            let table = find_spin_table(&reader).unwrap();
            assert_eq!(
                validate_memory(&reader, table, 0x80200000, 0x1000000).is_ok(),
                allowed
            );
        }
    }

    #[test]
    fn retains_non_psci_power_domain_tuples_and_matching_names() {
        let mut dtb = test_dtb(|structure, strings| {
            let compatible = ensure_string_vec(strings, b"compatible");
            let phandle = ensure_string_vec(strings, b"phandle");
            let cells = ensure_string_vec(strings, b"#power-domain-cells");
            let domains = ensure_string_vec(strings, b"power-domains");
            let names = ensure_string_vec(strings, b"power-domain-names");
            let reg = ensure_string_vec(strings, b"reg");
            write_begin_node_vec(structure, b"firmware-service");
            write_prop_vec(structure, compatible, b"arm,psci-1.0\0");
            write_begin_node_vec(structure, b"cpu-domain");
            write_prop_vec(structure, phandle, &0x300u32.to_be_bytes());
            write_prop_vec(structure, cells, &0u32.to_be_bytes());
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);
            write_begin_node_vec(structure, b"voltage-domain");
            write_prop_vec(structure, phandle, &0x301u32.to_be_bytes());
            write_prop_vec(structure, cells, &1u32.to_be_bytes());
            write_be32_vec(structure, FDT_END_NODE);
            write_begin_node_vec(structure, b"cpus");
            write_begin_node_vec(structure, b"cpu@0");
            write_prop_vec(structure, reg, &0u32.to_be_bytes());
            write_prop_vec(
                structure,
                domains,
                &[
                    0x300u32.to_be_bytes(),
                    0x301u32.to_be_bytes(),
                    7u32.to_be_bytes(),
                ]
                .concat(),
            );
            write_prop_vec(structure, names, b"psci\0voltage\0");
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);
        });
        let reader = Reader::new(&dtb).unwrap();
        let mut output = vec![0; 16384];
        let patched = patch_fdt(&reader, &mut output, SpinTable { addr: 0x90000000 }).unwrap();
        let patched = Reader::new(patched).unwrap();
        let cpu = patched.find_path(b"/cpus/cpu@0").unwrap();
        assert_eq!(
            patched.prop(cpu, b"power-domains").unwrap(),
            [0x301u32.to_be_bytes(), 7u32.to_be_bytes()].concat()
        );
        assert_eq!(
            patched.prop(cpu, b"power-domain-names").unwrap(),
            b"voltage\0"
        );
        let service = patched.find_path(b"/firmware-service").unwrap();
        assert_eq!(patched.prop_str(service, b"status").unwrap(), b"disabled");
        replace_prop(
            &mut dtb,
            b"/voltage-domain",
            b"#power-domain-cells",
            &2u32.to_be_bytes(),
        );
        assert!(
            patch_fdt(
                &Reader::new(&dtb).unwrap(),
                &mut output,
                SpinTable { addr: 0x90000000 }
            )
            .is_err()
        );
    }

    fn msm8916_test_dtb(with_spin_table: bool) -> Vec<u8> {
        msm8916_test_dtb_with_method(with_spin_table, b"psci\0")
    }

    #[test]
    fn accepts_lk2nd_bypass_method_then_patches_to_spin_table() {
        let dtb = msm8916_test_dtb_with_method(true, b"pocketboot,msm8916-acc\0");
        let reader = Reader::new(&dtb).unwrap();
        assert_eq!(collect_cpus(&reader).unwrap().as_slice().len(), 4);
        let mut output = vec![0; 16384];
        let patched = patch_fdt(&reader, &mut output, find_spin_table(&reader).unwrap()).unwrap();
        let patched = Reader::new(patched).unwrap();
        for cpu in 0..4 {
            let node = patched
                .find_path(format!("/cpus/cpu@{cpu:x}").as_bytes())
                .unwrap();
            assert_eq!(
                patched.prop_str(node, b"enable-method").unwrap(),
                b"spin-table"
            );
        }
    }

    fn msm8916_test_dtb_with_method(with_spin_table: bool, method: &[u8]) -> Vec<u8> {
        test_dtb(|structure, strings| {
            let address_cells = ensure_string_vec(strings, b"#address-cells");
            let size_cells = ensure_string_vec(strings, b"#size-cells");
            let reg = ensure_string_vec(strings, b"reg");
            let ranges = ensure_string_vec(strings, b"ranges");
            let no_map = ensure_string_vec(strings, b"no-map");
            let compatible = ensure_string_vec(strings, b"compatible");
            let status = ensure_string_vec(strings, b"status");
            let phandle = ensure_string_vec(strings, b"phandle");
            let serial0 = ensure_string_vec(strings, b"serial0");
            let stdout_path = ensure_string_vec(strings, b"stdout-path");
            let device_type = ensure_string_vec(strings, b"device_type");
            let enable_method = ensure_string_vec(strings, b"enable-method");
            let power_domains = ensure_string_vec(strings, b"power-domains");
            let power_domain_names = ensure_string_vec(strings, b"power-domain-names");
            let qcom_acc = ensure_string_vec(strings, b"qcom,acc");
            let power_domain_cells = ensure_string_vec(strings, b"#power-domain-cells");
            let cpu_idle_states = ensure_string_vec(strings, b"cpu-idle-states");
            let qcom_saw = ensure_string_vec(strings, b"qcom,saw");
            let entry_method = ensure_string_vec(strings, b"entry-method");
            let idle_state_name = ensure_string_vec(strings, b"idle-state-name");

            write_prop_vec(structure, address_cells, &2u32.to_be_bytes());
            write_prop_vec(structure, size_cells, &2u32.to_be_bytes());

            write_begin_node_vec(structure, b"memory@80000000");
            write_prop_vec(structure, device_type, b"memory\0");
            let mut memory_reg = Vec::new();
            write_be64_vec(&mut memory_reg, 0x80000000);
            write_be64_vec(&mut memory_reg, 0x40000000);
            write_prop_vec(structure, reg, &memory_reg);
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"aliases");
            write_prop_vec(structure, serial0, b"/soc@0/serial@78b0000\0");
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"chosen");
            write_prop_vec(structure, stdout_path, b"serial0:115200n8\0");
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"reserved-memory");
            write_prop_vec(structure, address_cells, &2u32.to_be_bytes());
            write_prop_vec(structure, size_cells, &2u32.to_be_bytes());
            write_prop_vec(structure, ranges, &[]);
            if with_spin_table {
                write_begin_node_vec(structure, b"spin-table@90000000");
                let mut value = Vec::new();
                write_be64_vec(&mut value, 0x9000_0000);
                write_be64_vec(&mut value, 0x1000);
                write_prop_vec(structure, reg, &value);
                write_prop_vec(structure, compatible, b"pocketboot,spin-table-v1\0");
                write_prop_vec(structure, no_map, &[]);
                write_be32_vec(structure, FDT_END_NODE);
            }
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"cpus");
            write_prop_vec(structure, address_cells, &1u32.to_be_bytes());
            write_prop_vec(structure, size_cells, &0u32.to_be_bytes());
            for cpu in 0..4 {
                write_cpu_node(
                    structure,
                    device_type,
                    compatible,
                    reg,
                    enable_method,
                    power_domains,
                    power_domain_names,
                    qcom_acc,
                    qcom_saw,
                    cpu,
                    0x100 + cpu,
                    0x200,
                    cpu_idle_states,
                    method,
                );
            }

            write_begin_node_vec(structure, b"idle-states");
            write_prop_vec(structure, entry_method, b"psci\0");
            write_begin_node_vec(structure, b"cpu-sleep-0");
            write_prop_vec(structure, compatible, b"arm,idle-state\0");
            write_prop_vec(structure, idle_state_name, b"standalone-power-collapse\0");
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"firmware-cpu-service");
            write_prop_vec(structure, compatible, b"arm,psci-1.0\0");
            write_begin_node_vec(structure, b"power-domain-cpu");
            write_prop_vec(structure, phandle, &0x300u32.to_be_bytes());
            write_prop_vec(structure, power_domain_cells, &0u32.to_be_bytes());
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"soc@0");
            write_prop_vec(structure, address_cells, &1u32.to_be_bytes());
            write_prop_vec(structure, size_cells, &1u32.to_be_bytes());
            write_begin_node_vec(structure, b"serial@78b0000");
            write_prop_vec(
                structure,
                compatible,
                b"qcom,msm-uartdm-v1.4\0qcom,msm-uartdm\0",
            );
            let mut uart_reg = Vec::new();
            write_be32_vec(&mut uart_reg, 0x078b0000);
            write_be32_vec(&mut uart_reg, 0x200);
            write_prop_vec(structure, reg, &uart_reg);
            write_be32_vec(structure, FDT_END_NODE);

            for cpu in 0..4 {
                let base = 0x0b088000 + cpu * 0x10000;
                let name = format!("power-manager@{base:x}");
                write_acc_or_saw(
                    structure,
                    reg,
                    compatible,
                    status,
                    phandle,
                    name.as_bytes(),
                    base,
                    0x100 + cpu,
                );
            }
            write_acc_or_saw(
                structure,
                reg,
                compatible,
                status,
                phandle,
                b"power-manager@b099000",
                0x0b099000,
                0x200,
            );
            write_be32_vec(structure, FDT_END_NODE);
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn write_cpu_node(
        structure: &mut Vec<u8>,
        device_type: u32,
        compatible: u32,
        reg: u32,
        enable_method: u32,
        power_domains: u32,
        power_domain_names: u32,
        qcom_acc: u32,
        qcom_saw: u32,
        cpu: u32,
        acc: u32,
        saw: u32,
        cpu_idle_states: u32,
        method: &[u8],
    ) {
        let name = format!("cpu@{cpu:x}");
        write_begin_node_vec(structure, name.as_bytes());
        write_prop_vec(structure, device_type, b"cpu\0");
        write_prop_vec(structure, compatible, b"arm,cortex-a53\0");
        write_prop_vec(structure, reg, &cpu.to_be_bytes());
        write_prop_vec(structure, enable_method, method);
        write_prop_vec(structure, power_domains, &0x300u32.to_be_bytes());
        write_prop_vec(structure, power_domain_names, b"psci\0");
        write_prop_vec(structure, qcom_acc, &acc.to_be_bytes());
        write_prop_vec(structure, qcom_saw, &saw.to_be_bytes());
        write_prop_vec(structure, cpu_idle_states, &0x400u32.to_be_bytes());
        write_be32_vec(structure, FDT_END_NODE);
    }

    fn write_acc_or_saw(
        structure: &mut Vec<u8>,
        reg: u32,
        compatible: u32,
        status: u32,
        phandle: u32,
        name: &[u8],
        base: u32,
        phandle_value: u32,
    ) {
        write_begin_node_vec(structure, name);
        write_prop_vec(structure, compatible, b"qcom,msm8916-acc\0");
        let mut value = Vec::new();
        write_be32_vec(&mut value, base);
        write_be32_vec(&mut value, 0x1000);
        write_prop_vec(structure, reg, &value);
        write_prop_vec(structure, phandle, &phandle_value.to_be_bytes());
        write_prop_vec(structure, status, b"reserved\0");
        write_be32_vec(structure, FDT_END_NODE);
    }
}
