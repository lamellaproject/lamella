//! The ADIv6 debug port, which selects an access port by its address in the debug address space
//! (a DPv3), and the search for the access port that reaches the processor on one.

use crate::coresight::{self, ARM_ELEVEN_BIT, Class, DebugBasePointer, DeviceArchitecture};
use crate::{
    ABORT_CLEAR_STICKY, AP_CSW, AP_DRW, AP_TAR, Ack, ArmDap, CSW_WORD, CoreMemory, DP_ABORT,
    DP_BANK_MASK, DP_SELECT, DapAccess, ProbeError,
};

/// `DEVARCH.ARCHID` of a MEM-AP (IHI 0029G, Table B2-19).
const MEM_AP_ARCHID: u16 = 0x0a17;
/// `DDEVARCH.ARCHID` of the M-profile debug architecture: `ARCHVER` 2 and `ARCHPART` `0xA04`
/// (DDI 0553B.y, D1.2.35).
const M_PROFILE_DEBUG_ARCHID: u16 = 0x2a04;
/// `DDEVARCH`, in the System Control Space of an M-profile processor (DDI 0553B.y, D1.2.35).
const DDEVARCH: u32 = 0xe000_efbc;
/// Where a MEM-AP's register file starts in its 4 KB block of the debug address space: `CSW` at
/// `0xD00`, `TAR` at `0xD04` and `DRW` at `0xD0C` (IHI 0074E, C2.5, Table C2-9).
const MEM_AP_REGISTERS: u32 = 0xd00;
/// `SELECT1`, the upper word of an access port's address: DP offset `0x4` in bank 5 (IHI 0074E,
/// B2.2).
const DP_SELECT1: u8 = 0x4;
/// The `DPBANKSEL` that puts `SELECT1` at DP offset `0x4`.
const DP_BANK_SELECT1: u32 = 0x5;

impl<D: DapAccess> ArmDap<D> {
    /// The `SELECT` value of the MEM-AP that reaches the processor on this DPv3 port: the first MEM-AP
    /// the debug port's ROM table lists, in table order, whose `DDEVARCH` names the M-profile debug
    /// architecture. Its `SELECT` is its base in the debug address space plus `0xD00`, where its
    /// register file starts.
    ///
    /// **Nothing about a particular part is written here.** The access ports are the ones the debug
    /// port's own ROM table lists (`BASEPTR`, IHI 0074E D1.5), and the one chosen is the one that
    /// answers as a processor's debug unit, so a part that moves its ports or lists another kind
    /// first is read the same way. A port that faults is passed over, its sticky error cleared.
    ///
    /// # Errors
    /// When a debug port register cannot be read or written, and by name: a port whose base pointer
    /// names no components or names them above 4 GB, a ROM table that lists no MEM-AP, and one whose
    /// MEM-APs reach no M-profile processor.
    pub fn core_access_port(&mut self) -> Result<u32, ProbeError> {
        let walk = self.debug_port_walk()?;
        let ports: Vec<u32> = walk
            .components
            .iter()
            .filter(|component| {
                component.class == Class::CoreSight
                    && component.architecture.is_some_and(|architecture| {
                        architecture.architect == ARM_ELEVEN_BIT && architecture.archid == MEM_AP_ARCHID
                    })
            })
            .map(|component| component.address)
            .collect();
        if ports.is_empty() {
            return Err(ProbeError::Device("the debug port's ROM table lists no memory access port"));
        }
        for base in ports {
            let select = base.wrapping_add(MEM_AP_REGISTERS);
            match self.reaches_m_profile(select) {
                Ok(true) => return Ok(select),
                Ok(false) => {}
                Err(ProbeError::Ack(Ack::Fault)) => self.dap.write_dp(DP_ABORT, ABORT_CLEAR_STICKY)?,
                Err(error) => return Err(error),
            }
        }
        self.dap.write_dp(DP_SELECT, self.select)?;
        Err(ProbeError::Device(
            "no memory access port on the debug port reaches an M-profile processor's debug unit",
        ))
    }

    /// Walks the debug components this DPv3 port's base pointer names, through access-port accesses
    /// to the debug address space, with the debug domain powered up. The DP `SELECT` value is put
    /// back afterwards.
    ///
    /// # Errors
    /// When a debug port register cannot be read or written, and by name: a port that is not a DPv3,
    /// and a base pointer that names no components or names them above 4 GB. What the walk itself
    /// cannot read is in the walk's problems, as [`coresight::walk`] reports it.
    pub fn debug_port_walk(&mut self) -> Result<coresight::Walk, ProbeError> {
        self.power_up(0)?;
        let base = match self.debug_base_pointer()? {
            DebugBasePointer::At { address, address_bits } => {
                let Ok(base) = u32::try_from(address) else {
                    return Err(ProbeError::Device(
                        "the debug port's base pointer names its components above 4 GB",
                    ));
                };
                if address_bits > 32 {
                    self.write_select1(0)?;
                }
                base
            }
            DebugBasePointer::Absent => {
                return Err(ProbeError::Device(
                    "the debug port's base pointer names no debug components (BASEPTR0.VALID is clear)",
                ));
            }
            DebugBasePointer::NotDpv3 { .. } => {
                return Err(ProbeError::Device("the debug port is not a DPv3, so it has no base pointer"));
            }
        };
        let walk = coresight::walk(&mut DebugSpace { dap: &mut self.dap, selected: None }, base);
        self.dap.write_dp(DP_SELECT, self.select)?;
        Ok(walk)
    }

    /// Whether the MEM-AP whose register file `select` selects reaches an M-profile processor's
    /// debug unit: its `DDEVARCH` names the M-profile debug architecture, whatever its revision.
    fn reaches_m_profile(&mut self, select: u32) -> Result<bool, ProbeError> {
        self.dap.write_dp(DP_SELECT, select)?;
        self.dap.write_ap(AP_CSW, CSW_WORD)?;
        self.dap.write_ap(AP_TAR, DDEVARCH)?;
        let word = self.dap.read_ap(AP_DRW)?;
        Ok(DeviceArchitecture::decode(word).is_some_and(|architecture| {
            architecture.architect == ARM_ELEVEN_BIT && architecture.archid == M_PROFILE_DEBUG_ARCHID
        }))
    }

    /// Writes `SELECT1` and puts the `SELECT` value back, whatever the write did.
    fn write_select1(&mut self, value: u32) -> Result<(), ProbeError> {
        self.dap.write_dp(DP_SELECT, (self.select & !DP_BANK_MASK) | DP_BANK_SELECT1)?;
        let written = self.dap.write_dp(DP_SELECT1, value);
        let restored = self.dap.write_dp(DP_SELECT, self.select);
        written?;
        restored
    }
}

/// The debug address space of a DPv3 port, read word by word through access-port accesses: what
/// [`coresight::walk`] reads the port's ROM table and components through.
///
/// Each word is reached by writing its address, less its low four bits, to `SELECT` and reading
/// the access port register those bits name (IHI 0074E, A1.6). A read that faults clears the sticky
/// error before it is reported, since `STICKYERR` would fault every read after it and the walk goes
/// on past an unreadable component.
struct DebugSpace<'a, D: DapAccess> {
    dap: &'a mut D,
    /// The `SELECT` value last written, so the words of one 16-byte group cost one write.
    selected: Option<u32>,
}

impl<D: DapAccess> CoreMemory for DebugSpace<'_, D> {
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        let select = address & !0xf;
        if self.selected != Some(select) {
            self.dap.write_dp(DP_SELECT, select)?;
            self.selected = Some(select);
        }
        match self.dap.read_ap((address & 0xc) as u8) {
            Err(ProbeError::Ack(Ack::Fault)) => {
                self.dap.write_dp(DP_ABORT, ABORT_CLEAR_STICKY)?;
                Err(ProbeError::Ack(Ack::Fault))
            }
            read => read,
        }
    }

    fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
        Err(ProbeError::Debugger(
            "the debug address space is read, never written, while its components are found",
        ))
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        self.dap.set_reset(assert)
    }
}

#[cfg(test)]
mod tests {
    use crate::coresight::{CIDR0, DEVARCH, DEVID, DEVTYPE_OR_MEMTYPE, PIDR0, PIDR4};
    use crate::{Ack, ArmDap, DapAccess, ProbeError, TargetAccess};
    use std::collections::BTreeMap;

    /// An RP2350's SW-DP: Arm-designed, and DPv3.
    const RP2350_SW_DP: u32 = 0x4c01_3477;
    /// `CPUID`, in the processor's System Control Space.
    const CPUID: u32 = 0xe000_ed00;
    /// `DDEVARCH`, the processor debug unit's own architecture register (Armv8-M, DDI 0553B.y,
    /// D1.2.35).
    const DDEVARCH: u32 = 0xe000_efbc;
    /// The RP2350's SIO `CPUID`: the number of the core that reads it (RP2350 datasheet, 3.1.2).
    const SIO_CPUID: u32 = 0xd000_0000;
    /// What an RP2350's Cortex-M33 answers in `CPUID`.
    const CORTEX_M33_CPUID: u32 = 0x411f_d210;
    /// `DDEVARCH` for the Armv8-M debug architecture: ARCHITECT `0x23B`, PRESENT, ARCHVER 2,
    /// ARCHPART `0xA04` (DDI 0553B.y, D1.2.35).
    const ARMV8M_DDEVARCH: u32 = 0x4770_2a04;
    /// `DEVARCH` of a MEM-AP: ARCHITECT `0x23B`, PRESENT, ARCHID `0x0A17` (IHI 0029G, Table B2-19).
    const MEM_AP_DEVARCH: u32 = 0x4770_0a17;
    /// `DEVARCH` of a Class `0x9` ROM table: ARCHITECT `0x23B`, PRESENT, ARCHID `0x0AF7`.
    const ROM_TABLE_DEVARCH: u32 = 0x4770_0af7;

    /// One MEM-AP behind the fake port: its `CSW` and `TAR`, and the memory it reaches.
    #[derive(Default)]
    struct MemAp {
        csw: u32,
        tar: u32,
        memory: BTreeMap<u32, u32>,
    }

    /// What the fake port was asked, for the tests that pin the traffic.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        Connect,
        Wake,
        ReadDp(u8),
        WriteDp(u8, u32),
        ReadAp(u8),
        WriteAp(u8, u32),
    }

    /// A SW-DP and the debug address space behind it, as a DPv3 has one; given an earlier `DPIDR`, it
    /// stands in for an ADIv5 port in the test that pins that port's traffic.
    ///
    /// It models what the search depends on, from IHI 0074E: an access-port access addresses
    /// `SELECT1:SELECT[31:4]` plus the access's `A[3:2]` (A1.6, B2.2.11); `DPBANKSEL` is `SELECT[3:0]`
    /// and chooses what DP offsets `0x0` and `0x4` read (`DPIDR`, `DPIDR1`, `BASEPTR0`, `BASEPTR1`;
    /// `CTRL/STAT`, `SELECT1`); no access port answers until debug power is up; and a faulted access
    /// sets `STICKYERR`, which faults every later access-port access until `ABORT` clears it.
    struct SwDp {
        dpidr: u32,
        dpidr1: u32,
        baseptr0: u32,
        baseptr1: u32,
        select: u32,
        select1: u32,
        powered: bool,
        sticky: bool,
        /// The words of the debug address space that are not a MEM-AP's `CSW`, `TAR` or `DRW`: ROM
        /// tables and identification blocks. Read only, as they are on a part.
        space: BTreeMap<u64, u32>,
        /// The MEM-APs, by the base of their 4 KB block in the debug address space.
        ports: BTreeMap<u64, MemAp>,
        /// Whether the port is DORMANT, answering no transfer until it is woken.
        dormant: bool,
        /// Whether this probe can send the wake ([`DapAccess::wake_from_dormant`]).
        wakes: bool,
        /// Whether the port answers every DP read with `WAIT`.
        waits: bool,
        log: Vec<Op>,
    }

    impl SwDp {
        /// A port answering `dpidr`, with a 32-bit debug address space whose base pointer names
        /// address 0, and nothing in it yet.
        fn new(dpidr: u32) -> SwDp {
            SwDp {
                dpidr,
                dpidr1: 32,
                baseptr0: 0x0000_0001,
                baseptr1: 0,
                select: 0,
                select1: 0,
                powered: false,
                sticky: false,
                space: BTreeMap::new(),
                ports: BTreeMap::new(),
                dormant: false,
                wakes: true,
                waits: false,
                log: Vec::new(),
            }
        }

        /// A Class `0x9` identification block at `base`, Arm-designed, with `devarch`.
        fn identified(mut self, base: u64, devarch: u32) -> SwDp {
            let words = [
                (CIDR0, 0x0d),
                (CIDR0 + 4, 0x90),
                (CIDR0 + 8, 0x05),
                (CIDR0 + 12, 0xb1),
                (PIDR0, 0x00),
                (PIDR0 + 4, 0xb0),
                (PIDR0 + 8, 0x0b),
                (PIDR0 + 12, 0x00),
                (PIDR4, 0x04),
                (DEVARCH, devarch),
                (DEVID, 0),
                (DEVTYPE_OR_MEMTYPE, 0),
            ];
            for (offset, word) in words {
                self.space.insert(base + u64::from(offset), word);
            }
            self
        }

        /// A Class `0x9` ROM table at `base` listing `entries`, then the entry that ends it.
        fn rom_table(mut self, base: u64, entries: &[u32]) -> SwDp {
            for (index, &entry) in entries.iter().chain(&[0]).enumerate() {
                self.space.insert(base + 4 * index as u64, entry);
            }
            self.identified(base, ROM_TABLE_DEVARCH)
        }

        /// A MEM-AP whose 4 KB block starts at `base`, reaching the words of `memory`.
        fn mem_ap(mut self, base: u64, memory: &[(u32, u32)]) -> SwDp {
            self.ports.insert(
                base,
                MemAp { memory: memory.iter().copied().collect(), ..MemAp::default() },
            );
            self.identified(base, MEM_AP_DEVARCH)
        }

        /// A component that is not an access port.
        fn other(self, base: u64) -> SwDp {
            self.identified(base, 0)
        }

        /// The debug address an access-port access to `a` reaches through the current selection.
        fn ap_address(&self, a: u8) -> u64 {
            (u64::from(self.select1) << 32) | u64::from(self.select & !0xf) | u64::from(a & 0xc)
        }

        /// A faulted access: `STICKYERR` is set and stays set until `ABORT` clears it.
        fn fault<T>(&mut self) -> Result<T, ProbeError> {
            self.sticky = true;
            Err(ProbeError::Ack(Ack::Fault))
        }
    }

    impl DapAccess for SwDp {
        fn connect(&mut self) -> Result<(), ProbeError> {
            self.log.push(Op::Connect);
            Ok(())
        }

        fn read_dp(&mut self, address: u8) -> Result<u32, ProbeError> {
            self.log.push(Op::ReadDp(address));
            if self.dormant {
                return Err(ProbeError::Ack(Ack::NoAck));
            }
            if self.waits {
                return Err(ProbeError::Ack(Ack::Wait));
            }
            Ok(match (address, self.select & 0xf) {
                (0x0, 0) => self.dpidr,
                (0x0, 1) => self.dpidr1,
                (0x0, 2) => self.baseptr0,
                (0x0, 3) => self.baseptr1,
                (0x4, 0) => {
                    let power = if self.powered { 0xf000_0000 } else { 0 };
                    power | if self.sticky { 0x20 } else { 0 }
                }
                (0x4, 5) => self.select1,
                _ => 0,
            })
        }

        fn write_dp(&mut self, address: u8, value: u32) -> Result<(), ProbeError> {
            self.log.push(Op::WriteDp(address, value));
            if self.dormant {
                return Err(ProbeError::Ack(Ack::NoAck));
            }
            match (address, self.select & 0xf) {
                (0x0, _) if value & 0x4 != 0 => self.sticky = false,
                (0x4, 0) if value & 0x5000_0000 == 0x5000_0000 => self.powered = true,
                (0x4, 5) => self.select1 = value,
                (0x8, _) => self.select = value,
                _ => {}
            }
            Ok(())
        }

        fn read_ap(&mut self, a: u8) -> Result<u32, ProbeError> {
            self.log.push(Op::ReadAp(a));
            if self.dormant {
                return Err(ProbeError::Ack(Ack::NoAck));
            }
            if !self.powered || self.sticky {
                return self.fault();
            }
            let address = self.ap_address(a);
            let (block, offset) = (address & !0xfff, address & 0xfff);
            if let Some(port) = self.ports.get_mut(&block) {
                let answer = match offset {
                    0xd00 => Some(Some(port.csw)),
                    0xd04 => Some(Some(port.tar)),
                    0xd0c => {
                        let word = port.memory.get(&port.tar).copied();
                        if word.is_some() && port.csw & 0x30 == 0x10 {
                            port.tar = port.tar.wrapping_add(4);
                        }
                        Some(word)
                    }
                    _ => None,
                };
                match answer {
                    Some(Some(word)) => return Ok(word),
                    Some(None) => return self.fault(),
                    None => {}
                }
            }
            match self.space.get(&address) {
                Some(&word) => Ok(word),
                None => self.fault(),
            }
        }

        fn write_ap(&mut self, a: u8, value: u32) -> Result<(), ProbeError> {
            self.log.push(Op::WriteAp(a, value));
            if self.dormant {
                return Err(ProbeError::Ack(Ack::NoAck));
            }
            if !self.powered || self.sticky {
                return self.fault();
            }
            let address = self.ap_address(a);
            let (block, offset) = (address & !0xfff, address & 0xfff);
            if let Some(port) = self.ports.get_mut(&block) {
                match offset {
                    0xd00 => port.csw = value,
                    0xd04 => port.tar = value,
                    0xd0c => {
                        port.memory.insert(port.tar, value);
                        if port.csw & 0x30 == 0x10 {
                            port.tar = port.tar.wrapping_add(4);
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }

        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            Ok(0)
        }

        fn wake_from_dormant(&mut self) -> Result<bool, ProbeError> {
            self.log.push(Op::Wake);
            if self.wakes {
                self.dormant = false;
            }
            Ok(self.wakes)
        }
    }

    /// The RP2350's debug address space as its datasheet's Figure 10 lays it out: the ROM table at
    /// 0, the two cores' AHB-APs at `0x2000` and `0x4000`, then the timestamp generator, the ATB
    /// funnel, the TPIU and the CTI, and the RISC-V cores' APB-AP at `0xa000`.
    fn rp2350() -> SwDp {
        SwDp::new(RP2350_SW_DP)
            .rom_table(0x0, &[0x2003, 0x4003, 0x6003, 0x7003, 0x8003, 0x9003, 0xa003])
            .mem_ap(0x2000, &[(CPUID, CORTEX_M33_CPUID), (DDEVARCH, ARMV8M_DDEVARCH), (SIO_CPUID, 0)])
            .mem_ap(0x4000, &[(CPUID, CORTEX_M33_CPUID), (DDEVARCH, ARMV8M_DDEVARCH), (SIO_CPUID, 1)])
            .other(0x6000)
            .other(0x7000)
            .other(0x8000)
            .other(0x9000)
            .mem_ap(0xa000, &[])
    }

    /// **THE FAKE REPRODUCES THE DEFECT FIRST**: through ADIv5's selection of access port 0, every
    /// word this RP2350 reads is the ROM table's fourth entry, `0x00007003`. A fake that did not
    /// reproduce the defect could not show it fixed.
    #[test]
    fn adiv5s_selection_reads_the_rom_tables_fourth_entry_for_every_word() {
        let mut arm = ArmDap::new(rp2350());
        arm.connect().expect("connect");
        arm.init_mem_select(0).expect("ADIv5's selection of access port 0");
        for address in [CPUID, 0xe000_2000, 0xe000_edf0] {
            assert_eq!(arm.read_word(address).expect("a word"), 0x0000_7003, "{address:#010x}");
        }
    }

    /// **ON A DPv3, MEMORY ACCESS REACHES THE PROCESSOR**: `init_mem` finds core 0's access port from
    /// the debug port's own ROM table, so `CPUID` is the Cortex-M33's and the SIO answers core 0.
    #[test]
    fn init_mem_on_a_dpv3_reaches_core_0() {
        let mut arm = ArmDap::new(rp2350());
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.read_word(CPUID).expect("CPUID"), CORTEX_M33_CPUID);
        assert_eq!(arm.read_word(SIO_CPUID).expect("SIO CPUID"), 0, "core 0's port");
        assert_eq!(arm.select(), 0x2d00, "the AHB-AP at 0x2000, and its register file at 0xD00");
    }

    /// The memory initialization of a port that was never connected reads `DPIDR` itself.
    #[test]
    fn init_mem_without_a_connect_reads_the_ports_id_first() {
        let mut arm = ArmDap::new(rp2350());
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.inner().log[0], Op::ReadDp(0x0), "{:?}", arm.inner().log);
        assert_eq!(arm.read_word(CPUID).expect("CPUID"), CORTEX_M33_CPUID);
    }

    /// **NO PART'S ADDRESS IS WRITTEN IN**: a part that puts its processor's access port elsewhere,
    /// or lists another component first, is read the same way.
    #[test]
    fn a_port_is_found_wherever_the_rom_table_puts_it() {
        let moved = SwDp::new(RP2350_SW_DP)
            .rom_table(0x0, &[0x7003, 0x5003])
            .other(0x7000)
            .mem_ap(0x5000, &[(CPUID, CORTEX_M33_CPUID), (DDEVARCH, ARMV8M_DDEVARCH)]);
        let mut arm = ArmDap::new(moved);
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.select(), 0x5d00);
        assert_eq!(arm.read_word(CPUID).expect("CPUID"), CORTEX_M33_CPUID);
    }

    /// **A PORT THAT REACHES NO M-PROFILE PROCESSOR IS PASSED OVER**, whether its `DDEVARCH` read
    /// faults -- a port with nothing at that address, as the RISC-V cores' APB-AP has -- or answers
    /// another architecture. The fault's sticky error is cleared, so the ports after it are read.
    #[test]
    fn a_port_that_faults_or_reaches_another_processor_is_passed_over() {
        let another = 0x4770_0001;
        let others_first = SwDp::new(RP2350_SW_DP)
            .rom_table(0x0, &[0xa003, 0xb003, 0x2003])
            .mem_ap(0xa000, &[])
            .mem_ap(0xb000, &[(DDEVARCH, another)])
            .mem_ap(0x2000, &[(CPUID, CORTEX_M33_CPUID), (DDEVARCH, ARMV8M_DDEVARCH)]);
        let mut arm = ArmDap::new(others_first);
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.select(), 0x2d00);
        assert_eq!(arm.read_word(CPUID).expect("CPUID, with no sticky error left"), CORTEX_M33_CPUID);
    }

    /// **A PORT WHERE NO PROCESSOR IS FOUND IS REFUSED BY NAME**, rather than served against words
    /// that are not the processor's.
    #[test]
    fn a_debug_port_with_no_m_profile_processor_is_refused_by_name() {
        let refusal = |port: SwDp| {
            let mut arm = ArmDap::new(port);
            arm.connect().expect("connect");
            match arm.init_mem().expect_err("refused") {
                ProbeError::Device(reason) => reason,
                other => panic!("refused by name, not {other:?}"),
            }
        };
        let only_riscv = SwDp::new(RP2350_SW_DP).rom_table(0x0, &[0xa003]).mem_ap(0xa000, &[]);
        assert!(refusal(only_riscv).contains("M-profile"));
        let no_ports = SwDp::new(RP2350_SW_DP).rom_table(0x0, &[0x7003]).other(0x7000);
        assert!(refusal(no_ports).contains("lists no memory access port"));
        let mut absent = rp2350();
        absent.baseptr0 = 0;
        assert!(refusal(absent).contains("BASEPTR0.VALID is clear"));
        let mut high = rp2350();
        high.baseptr1 = 1;
        assert!(refusal(high).contains("above 4 GB"));
    }

    /// **ON A PORT WIDER THAN 32 BITS, `SELECT1` IS WRITTEN BEFORE ANY COMPONENT IS READ**: it is the
    /// upper half of every access-port address and holds anything after a reset. A 32-bit port is not
    /// written it.
    #[test]
    fn a_wide_port_has_select1_written_before_its_components_are_read() {
        let mut wide = rp2350();
        wide.dpidr1 = 40;
        wide.select1 = 0xdead;
        let mut arm = ArmDap::new(wide);
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.inner().select1, 0);
        assert_eq!(arm.read_word(CPUID).expect("CPUID"), CORTEX_M33_CPUID);

        let mut arm = ArmDap::new(rp2350());
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        let banked = |op: &Op| matches!(op, Op::WriteDp(0x8, select) if select & 0xf == 5);
        assert!(!arm.inner().log.iter().any(banked), "{:?}", arm.inner().log);
    }

    /// **AN ADIv5 PORT SEES THE TRAFFIC IT ALWAYS DID**: the connect, then AP 0 selected and powered,
    /// with nothing read to find a port and no wake.
    #[test]
    fn an_adiv5_port_is_initialized_with_the_traffic_it_always_had() {
        let mut arm = ArmDap::new(SwDp::new(0x2ba0_1477));
        arm.connect().expect("connect");
        arm.init_mem().expect("init_mem");
        assert_eq!(
            arm.inner().log,
            vec![
                Op::Connect,
                Op::ReadDp(0x0),
                Op::WriteDp(0x0, 0x1e),
                Op::WriteDp(0x8, 0),
                Op::WriteDp(0x4, 0x5000_0000),
                Op::ReadDp(0x4),
                Op::WriteAp(0x0, 0x2300_0052),
            ]
        );
    }

    /// **A DORMANT PORT IS WOKEN, AND THEN READ**: it answers nothing to the switch, so the probe is
    /// asked for the wake and `DPIDR` is read again.
    #[test]
    fn a_dormant_port_is_woken_and_then_read() {
        let mut dormant = rp2350();
        dormant.dormant = true;
        let mut arm = ArmDap::new(dormant);
        arm.connect().expect("woken");
        assert_eq!(arm.inner().log, vec![Op::Connect, Op::ReadDp(0x0), Op::Wake, Op::ReadDp(0x0)]);
        arm.init_mem().expect("init_mem");
        assert_eq!(arm.read_word(CPUID).expect("CPUID"), CORTEX_M33_CPUID);
    }

    /// **ONLY A PORT THAT ANSWERED NOTHING IS SENT THE WAKE**: one that answers at once is not, and
    /// neither is one that answered `WAIT`, which is awake.
    #[test]
    fn a_port_that_answers_is_never_sent_the_wake() {
        let mut arm = ArmDap::new(rp2350());
        arm.connect().expect("connect");
        assert!(!arm.inner().log.contains(&Op::Wake), "{:?}", arm.inner().log);

        let mut waiting = rp2350();
        waiting.waits = true;
        let mut arm = ArmDap::new(waiting);
        assert!(matches!(arm.connect(), Err(ProbeError::Ack(Ack::Wait))));
        assert!(!arm.inner().log.contains(&Op::Wake), "{:?}", arm.inner().log);
    }

    /// **A PROBE THAT CANNOT SEND THE WAKE REPORTS THE PORT'S SILENCE AS IT FOUND IT**: its
    /// `Ok(false)` is an answer, and the error is the one the port gave.
    #[test]
    fn a_probe_that_cannot_wake_reports_the_ports_silence_as_it_found_it() {
        let mut dormant = rp2350();
        dormant.dormant = true;
        dormant.wakes = false;
        let mut arm = ArmDap::new(dormant);
        assert!(matches!(arm.connect(), Err(ProbeError::Ack(Ack::NoAck))));
        assert_eq!(arm.inner().log, vec![Op::Connect, Op::ReadDp(0x0), Op::Wake]);
    }

    /// The wake reaches the probe through a borrow too, rather than the trait's `Ok(false)`.
    #[test]
    fn a_borrowed_probe_forwards_the_wake() {
        fn through_the_seam<D: DapAccess>(mut probe: D) -> bool {
            probe.wake_from_dormant().expect("the wake")
        }
        let mut port = rp2350();
        port.dormant = true;
        assert!(through_the_seam(&mut port));
        assert!(!port.dormant);
        assert_eq!(port.log, vec![Op::Wake]);
    }
}
