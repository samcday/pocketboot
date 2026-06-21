pub mod uart {
    const UART2_BASE: usize = 0x1382_0000;
    const UTXH: usize = 0x20;
    const UFSTAT: usize = 0x18;
    const TX_FULL: u32 = 1 << 24;

    pub fn writeln(message: &str) {
        for byte in message.bytes() {
            write_byte(byte);
        }
        write_byte('\r' as _);
        write_byte('\n' as _);
    }

    fn write_byte(byte: u8) {
        wait_for_tx_space();
        unsafe {
            ((UART2_BASE + UTXH) as *mut u8).write_volatile(byte);
        }
    }

    fn wait_for_tx_space() {
        while read32(UFSTAT) & TX_FULL != 0 {
            core::hint::spin_loop();
        }
    }

    fn read32(offset: usize) -> u32 {
        unsafe { ((UART2_BASE + offset) as *const u32).read_volatile() }
    }
}

mod clock {
    const CMU_PERI_BASE: usize = 0x101f_0000;
    const CLK_CON_GAT_PERI_I2C: usize = 0x0810;
    const BUSP1_PERIC0_HCLK: u32 = 1 << 3;
    const GPIO2_PCLK: u32 = 1 << 7;
    const GPIO5_PCLK: u32 = 1 << 8;
    const GPIO6_PCLK: u32 = 1 << 9;
    const GPIO7_PCLK: u32 = 1 << 10;
    const I2C4_PCLK: u32 = 1 << 17;
    const GATE_SETTLE_SPINS: usize = 1024;

    pub fn enable_i2c4() {
        set32(
            CMU_PERI_BASE + CLK_CON_GAT_PERI_I2C,
            BUSP1_PERIC0_HCLK | GPIO2_PCLK | GPIO5_PCLK | GPIO6_PCLK | GPIO7_PCLK | I2C4_PCLK,
        );

        for _ in 0..GATE_SETTLE_SPINS {
            core::hint::spin_loop();
        }
    }

    fn set32(address: usize, bits: u32) {
        unsafe {
            let register = address as *mut u32;
            register.write_volatile(register.read_volatile() | bits);
        }
    }
}

mod pinctrl {
    const PINCTRL_TOP_BASE: usize = 0x139b_0000;
    const GPC1_OFFSET: usize = 0x040;
    const CON: usize = 0x00;
    const PUD: usize = 0x08;
    const DRV: usize = 0x0c;
    const CON_PDN: usize = 0x10;
    const PUD_PDN: usize = 0x14;

    const GPC1_0_1_CON_MASK: u32 = 0xff;
    const GPC1_0_1_CON_I2C4: u32 = 0x22;
    const GPC1_0_1_PUD_MASK: u32 = 0x0f;
    const GPC1_0_1_PULL_UP: u32 = 0x0f;
    const GPC1_0_1_DRV_MASK: u32 = 0x3f;
    const GPC1_0_1_FAST_SR1: u32 = 0x00;
    const GPC1_0_1_PDN_PREV: u32 = 0x0f;

    pub fn configure_i2c4() {
        update32(CON, GPC1_0_1_CON_MASK, GPC1_0_1_CON_I2C4);
        update32(PUD, GPC1_0_1_PUD_MASK, GPC1_0_1_PULL_UP);
        update32(DRV, GPC1_0_1_DRV_MASK, GPC1_0_1_FAST_SR1);
        update32(CON_PDN, GPC1_0_1_PUD_MASK, GPC1_0_1_PDN_PREV);
        update32(PUD_PDN, GPC1_0_1_PUD_MASK, GPC1_0_1_PULL_UP);
    }

    fn update32(offset: usize, mask: u32, value: u32) {
        unsafe {
            let register = (PINCTRL_TOP_BASE + GPC1_OFFSET + offset) as *mut u32;
            let current = register.read_volatile();
            register.write_volatile((current & !mask) | (value & mask));
        }
    }
}

mod i2c {
    const I2C4_BASE: usize = 0x1387_0000;
    const IICCON: usize = 0x00;
    const IICSTAT: usize = 0x04;
    const IICADD: usize = 0x08;
    const IICDS: usize = 0x0c;
    const IICLC: usize = 0x10;

    const IICCON_ACKEN: u32 = 1 << 7;
    const IICCON_TXDIV_512: u32 = 1 << 6;
    const IICCON_IRQEN: u32 = 1 << 5;
    const IICCON_IRQPEND: u32 = 1 << 4;
    const IICCON_SCALE: u32 = 7;

    const IICSTAT_MASTER_TX: u32 = 3 << 6;
    const IICSTAT_START: u32 = 1 << 5;
    const IICSTAT_TXRXEN: u32 = 1 << 4;
    const IICSTAT_ARBITR: u32 = 1 << 3;
    const IICSTAT_LASTBIT: u32 = 1 << 0;

    const IICLC_SDA_DELAY5: u32 = 1 << 0;
    const IICLC_FILTER_ON: u32 = 1 << 2;

    const CON_INIT: u32 = IICCON_ACKEN | IICCON_TXDIV_512 | IICCON_IRQEN | IICCON_SCALE;
    const IRQ_TIMEOUT_SPINS: usize = 500_000;
    const BUS_IDLE_TIMEOUT_SPINS: usize = 500_000;
    const TX_SETUP_SPINS: usize = 128;

    pub const I2C4: Bus = Bus { base: I2C4_BASE };

    pub struct Bus {
        base: usize,
    }

    #[derive(Clone, Copy)]
    pub enum Error {
        IdleTimeout,
        AddressTimeout,
        AddressNoAck,
        AddressArbitrationLost,
        DataTimeout,
        DataNoAck,
        DataArbitrationLost,
    }

    #[derive(Clone, Copy)]
    enum ByteError {
        Timeout,
        NoAck,
        ArbitrationLost,
    }

    impl Bus {
        pub fn init(&self) {
            self.write32(IICADD, 0);
            self.write32(IICCON, 0);
            self.write32(IICSTAT, 0);
            self.write32(IICLC, IICLC_FILTER_ON | IICLC_SDA_DELAY5);
            self.write32(IICCON, CON_INIT);
        }

        pub fn write_reg(&self, slave_addr: u8, register: u8, value: u8) -> Result<(), Error> {
            self.write(slave_addr, &[register, value])
        }

        fn write(&self, slave_addr: u8, bytes: &[u8]) -> Result<(), Error> {
            let result = self.write_inner(slave_addr, bytes);
            if result.is_err() {
                self.stop();
            }
            result
        }

        fn write_inner(&self, slave_addr: u8, bytes: &[u8]) -> Result<(), Error> {
            self.wait_bus_idle()?;
            self.write32(IICCON, self.read32(IICCON) | IICCON_ACKEN | IICCON_IRQEN);

            let stat = IICSTAT_MASTER_TX | IICSTAT_TXRXEN;
            self.write32(IICSTAT, stat);
            self.write32(IICDS, (slave_addr << 1) as u32);
            short_delay();
            self.write32(IICSTAT, stat | IICSTAT_START);

            self.wait_byte_done().map_err(Error::from_address_error)?;
            for byte in bytes {
                self.write32(IICDS, *byte as u32);
                short_delay();
                self.clear_irqpend();
                self.wait_byte_done().map_err(Error::from_data_error)?;
            }

            self.stop();
            Ok(())
        }

        fn wait_byte_done(&self) -> Result<(), ByteError> {
            for _ in 0..IRQ_TIMEOUT_SPINS {
                if self.read32(IICCON) & IICCON_IRQPEND == 0 {
                    core::hint::spin_loop();
                    continue;
                }

                let stat = self.read32(IICSTAT);
                if stat & IICSTAT_ARBITR != 0 {
                    return Err(ByteError::ArbitrationLost);
                }
                if stat & IICSTAT_LASTBIT != 0 {
                    return Err(ByteError::NoAck);
                }
                return Ok(());
            }

            Err(ByteError::Timeout)
        }

        fn wait_bus_idle(&self) -> Result<(), Error> {
            for _ in 0..BUS_IDLE_TIMEOUT_SPINS {
                if self.read32(IICSTAT) & IICSTAT_START == 0 {
                    return Ok(());
                }
                core::hint::spin_loop();
            }

            Err(Error::IdleTimeout)
        }

        fn stop(&self) {
            self.write32(IICSTAT, self.read32(IICSTAT) & !IICSTAT_START);
            self.clear_irqpend();

            for _ in 0..BUS_IDLE_TIMEOUT_SPINS {
                if self.read32(IICSTAT) & IICSTAT_START == 0 {
                    break;
                }
                core::hint::spin_loop();
            }

            self.disable_bus();
        }

        fn disable_bus(&self) {
            self.write32(IICSTAT, self.read32(IICSTAT) & !IICSTAT_TXRXEN);
            self.write32(
                IICCON,
                self.read32(IICCON) & !(IICCON_IRQEN | IICCON_IRQPEND | IICCON_ACKEN),
            );
        }

        fn clear_irqpend(&self) {
            self.write32(IICCON, self.read32(IICCON) & !IICCON_IRQPEND);
        }

        fn read32(&self, offset: usize) -> u32 {
            unsafe { ((self.base + offset) as *const u32).read_volatile() }
        }

        fn write32(&self, offset: usize, value: u32) {
            unsafe {
                ((self.base + offset) as *mut u32).write_volatile(value);
            }
        }
    }

    impl Error {
        fn from_address_error(error: ByteError) -> Self {
            match error {
                ByteError::Timeout => Self::AddressTimeout,
                ByteError::NoAck => Self::AddressNoAck,
                ByteError::ArbitrationLost => Self::AddressArbitrationLost,
            }
        }

        fn from_data_error(error: ByteError) -> Self {
            match error {
                ByteError::Timeout => Self::DataTimeout,
                ByteError::NoAck => Self::DataNoAck,
                ByteError::ArbitrationLost => Self::DataArbitrationLost,
            }
        }
    }

    fn short_delay() {
        for _ in 0..TX_SETUP_SPINS {
            core::hint::spin_loop();
        }
    }
}

#[cfg(feature = "device-exynos7870-j7xelte")]
pub mod j7xelte {
    use super::{clock, i2c, pinctrl};

    const S2MU005_MUIC_I2C_ADDR: u8 = 0x3d;
    const S2MU005_MUIC_CTRL1: u8 = 0xb2;
    const S2MU005_MUIC_SWCTRL: u8 = 0xb5;
    const S2MU005_MUIC_CTRL_MANUAL: u8 = 0x13;
    const S2MU005_MUIC_SW_UART: u8 = 0x48;

    pub fn route_muic_to_uart() -> bool {
        clock::enable_i2c4();
        pinctrl::configure_i2c4();

        let bus = i2c::I2C4;
        bus.init();

        if bus
            .write_reg(
                S2MU005_MUIC_I2C_ADDR,
                S2MU005_MUIC_CTRL1,
                S2MU005_MUIC_CTRL_MANUAL,
            )
            .is_err()
        {
            return false;
        }

        bus.write_reg(
            S2MU005_MUIC_I2C_ADDR,
            S2MU005_MUIC_SWCTRL,
            S2MU005_MUIC_SW_UART,
        )
        .is_ok()
    }
}
