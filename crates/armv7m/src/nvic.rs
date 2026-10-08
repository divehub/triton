// Ported from Renode 1.17.0 src/Emulator/Cores/Arm-M/NVIC.cs (MIT License, Copyright (c) Antmicro).
//
//! Nested vectored interrupt controller state: exception pending / active /
//! enabled / running sets, priorities with `AIRCR.PRIGROUP`, BASEPRI / PRIMASK /
//! FAULTMASK masking, synchronous-fault escalation and lockup. TrustZone,
//! banked exceptions and the PMSAv8 paths of the Renode model are not ported
//! (Cortex-M4 has none of them).
//!
//! Exception numbers follow the architecture: 1 = Reset, 2 = NMI, 3 = HardFault,
//! 4 = MemManage, 5 = BusFault, 6 = UsageFault, 11 = SVCall, 12 = DebugMonitor,
//! 14 = PendSV, 15 = SysTick, 16.. = external interrupts (IRQ n is exception
//! 16 + n).

pub const EXC_RESET: usize = 1;
pub const EXC_NMI: usize = 2;
pub const EXC_HARDFAULT: usize = 3;
pub const EXC_MEMMANAGE: usize = 4;
pub const EXC_BUSFAULT: usize = 5;
pub const EXC_USAGEFAULT: usize = 6;
pub const EXC_SVCALL: usize = 11;
pub const EXC_DEBUGMON: usize = 12;
pub const EXC_PENDSV: usize = 14;
pub const EXC_SYSTICK: usize = 15;
pub const EXC_IRQ0: usize = 16;

/// Maximum number of exceptions (16 system + 240 external).
pub const MAX_EXC: usize = 256;
const WORDS: usize = MAX_EXC / 32 + 1;

/// Result of raising a synchronous fault (Renode `SynchronousFaultResult`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncFault {
    Pending,
    Lockup,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bits([u32; WORDS]);

impl Bits {
    #[inline]
    fn get(&self, i: usize) -> bool {
        self.0[i >> 5] >> (i & 31) & 1 != 0
    }
    #[inline]
    fn set(&mut self, i: usize) {
        self.0[i >> 5] |= 1 << (i & 31);
    }
    #[inline]
    fn clear(&mut self, i: usize) {
        self.0[i >> 5] &= !(1 << (i & 31));
    }
    fn clear_all(&mut self) {
        self.0 = [0; WORDS];
    }
    /// 32 bits of the external-IRQ register `k` (IRQ 32k .. 32k+31).
    fn irq_reg(&self, k: usize) -> u32 {
        if k >= 8 {
            return 0;
        }
        (self.0[k] >> 16) | (self.0[k + 1] << 16)
    }
}

#[derive(Clone, Debug)]
pub struct Nvic {
    num_irqs: u32,
    priority_mask: u8,
    pending: Bits,
    enabled: Bits,
    active: Bits,
    /// Level of the external line is high (Renode `IRQState.Running`).
    running: Bits,
    prio: [u8; MAX_EXC],
    active_stack: Vec<u16>,
    pub prigroup: u32,
    pub primask: bool,
    pub faultmask: bool,
    /// BASEPRI as written (readback) and masked with the priority mask (priority compare).
    pub basepri_raw: u8,
    basepri: u8,
    /// SHCSR MEMFAULTENA / BUSFAULTENA / USGFAULTENA (indices exception - 4).
    pub fault_enable: [bool; 3],
    pub hardfault_forced: bool,
    pub hardfault_vecttbl: bool,
    pub locked_up: bool,
    pub sevonpend: bool,
    /// Set when an exception becomes pending while SEVONPEND is set; consumed by the core.
    pub sev_pending_event: bool,
    // Cached results of the last `find_pending` (Renode: IRQ line, MaskedInterruptPresent).
    pub irq_line: bool,
    pub masked_present: bool,
    pub pending_exc: u16,
    /// The IRQ line went low -> high since the flag was last cleared. Renode: `IRQ.Set(true)`
    /// reaches tlib as `tlib_set_irq`, which also sets the CPU `exit_request`.
    pub irq_rose: bool,
}

impl Nvic {
    pub fn new(num_irqs: u32, priority_mask: u8) -> Self {
        let mut n = Nvic {
            num_irqs: num_irqs.min(240),
            priority_mask,
            pending: Bits::default(),
            enabled: Bits::default(),
            active: Bits::default(),
            running: Bits::default(),
            prio: [0; MAX_EXC],
            active_stack: Vec::with_capacity(64),
            prigroup: 0,
            primask: false,
            faultmask: false,
            basepri_raw: 0,
            basepri: 0,
            fault_enable: [false; 3],
            hardfault_forced: false,
            hardfault_vecttbl: false,
            locked_up: false,
            sevonpend: false,
            sev_pending_event: false,
            irq_line: false,
            masked_present: false,
            pending_exc: 0,
            irq_rose: false,
        };
        n.reset();
        n
    }

    pub fn reset(&mut self) {
        self.pending.clear_all();
        self.enabled.clear_all();
        self.active.clear_all();
        self.running.clear_all();
        for i in 0..16 {
            self.enabled.set(i);
        }
        self.prio = [0; MAX_EXC];
        self.active_stack.clear();
        self.prigroup = 0;
        self.primask = false;
        self.faultmask = false;
        self.basepri_raw = 0;
        self.basepri = 0;
        self.fault_enable = [false; 3];
        self.hardfault_forced = false;
        self.hardfault_vecttbl = false;
        self.locked_up = false;
        self.sevonpend = false;
        self.sev_pending_event = false;
        self.irq_line = false;
        self.masked_present = false;
        self.pending_exc = 0;
        self.irq_rose = false;
    }

    /// Drives the CPU IRQ output (`IRQ.Set(level)`); records rising edges.
    #[inline]
    fn drive_irq(&mut self, level: bool) {
        if level && !self.irq_line {
            self.irq_rose = true;
        }
        self.irq_line = level;
    }

    pub fn num_irqs(&self) -> u32 {
        self.num_irqs
    }

    pub fn priority_mask(&self) -> u8 {
        self.priority_mask
    }

    // --- priorities ------------------------------------------------------------

    /// BASEPRI written by the core (`MSR BASEPRI`): stored raw, masked for comparisons.
    pub fn set_basepri(&mut self, v: u8) {
        self.basepri_raw = v;
        self.basepri = v & self.priority_mask;
    }

    #[inline]
    fn apply_grouping(&self, p: i32) -> i32 {
        let m = !((1i32 << (self.prigroup + 1)) - 1);
        p & m
    }

    /// Priority with fixed priorities for Reset/NMI/HardFault and PRIGROUP applied
    /// (`GetExceptionPriority(n, groupPriority: true)`).
    pub fn group_priority(&self, exc: usize) -> i32 {
        match exc {
            EXC_RESET => -4,
            EXC_NMI => -2,
            EXC_HARDFAULT => -1,
            _ => self.apply_grouping(self.prio[exc] as i32),
        }
    }

    /// Full (sub-priority included) priority for choosing among pending exceptions.
    fn full_priority(&self, exc: usize) -> i32 {
        match exc {
            EXC_RESET => -4,
            EXC_NMI => -2,
            EXC_HARDFAULT => -1,
            _ => self.prio[exc] as i32,
        }
    }

    pub fn priority(&self, exc: usize) -> u8 {
        self.prio[exc]
    }

    /// Writes the priority byte of exception `exc` (masked with the implemented bits).
    pub fn set_priority(&mut self, exc: usize, v: u8) {
        if exc < MAX_EXC {
            self.prio[exc] = v & self.priority_mask;
        }
    }

    fn does_a_preempt_b(&self, a: usize, b: usize) -> bool {
        let pa = self.full_priority(a);
        let pb = self.full_priority(b);
        if pa != pb {
            return pa < pb;
        }
        a < b
    }

    fn raw_execution_priority(&self, ignored: Option<u16>) -> i32 {
        let mut p = 0x100;
        let mut skipped = false;
        for &a in self.active_stack.iter().rev() {
            if !skipped && ignored == Some(a) {
                skipped = true;
                continue;
            }
            p = p.min(self.group_priority(a as usize));
        }
        p
    }

    /// `GetRawExecutionPriority()` without boosts.
    pub fn raw_priority(&self) -> i32 {
        self.raw_execution_priority(None)
    }

    fn priority_boost(&self, ignore_primask: bool) -> i32 {
        let mut b = 0x100;
        if self.basepri != 0 {
            b = self.apply_grouping(self.basepri as i32);
        }
        if !ignore_primask && self.primask {
            b = b.min(0);
        }
        if self.faultmask {
            b = b.min(-1);
        }
        b
    }

    /// Execution priority (`GetExecutionPriority`): the lowest of the active
    /// exceptions' group priorities and the mask boosts.
    pub fn execution_priority(&self, ignore_primask: bool, ignored: Option<u16>) -> i32 {
        self.raw_execution_priority(ignored).min(self.priority_boost(ignore_primask))
    }

    // --- state queries ------------------------------------------------------------

    #[inline]
    pub fn is_pending(&self, exc: usize) -> bool {
        self.pending.get(exc)
    }
    #[inline]
    pub fn is_active(&self, exc: usize) -> bool {
        self.active.get(exc)
    }
    pub fn active_stack(&self) -> &[u16] {
        &self.active_stack
    }

    /// Top of the active exception stack (the current IPSR value), 0 in Thread mode.
    pub fn current_exception(&self) -> u16 {
        self.active_stack.last().copied().unwrap_or(0)
    }

    pub fn pending_irq_reg(&self, k: usize) -> u32 {
        self.pending.irq_reg(k)
    }
    pub fn enabled_irq_reg(&self, k: usize) -> u32 {
        self.enabled.irq_reg(k)
    }
    pub fn active_irq_reg(&self, k: usize) -> u32 {
        self.active.irq_reg(k)
    }

    fn is_candidate(&self, exc: usize) -> bool {
        self.pending.get(exc) && self.enabled.get(exc) && !self.active.get(exc)
    }

    // --- FindPendingInterrupt ------------------------------------------------------

    /// Side-effect free variant of `find_pending` (debugger / ICSR peek).
    pub fn peek_pending(&self) -> Option<u16> {
        self.select_pending().map(|r| r as u16)
    }

    fn select_pending(&self) -> Option<usize> {
        let preempt_needed = !self.active_stack.is_empty();
        let mut result: Option<usize> = None;
        for w in 0..WORDS {
            let mut bits = self.pending.0[w];
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let i = w * 32 + b;
                if i >= MAX_EXC {
                    break;
                }
                if self.locked_up && i != EXC_NMI {
                    continue;
                }
                if self.is_candidate(i) && result.map_or(true, |r| self.does_a_preempt_b(i, r)) {
                    result = Some(i);
                }
            }
        }
        if preempt_needed {
            if let Some(r) = result {
                if self.group_priority(r) >= self.raw_execution_priority(None) {
                    result = None;
                }
            }
        }
        result
    }

    /// Renode `FindPendingInterrupt`: recomputes the highest-priority pending
    /// exception and the cached IRQ line / masked-pending flags.
    pub fn find_pending(&mut self) -> Option<u16> {
        match self.select_pending() {
            None => {
                self.drive_irq(false);
                self.masked_present = false;
                self.pending_exc = 0;
                None
            }
            Some(r) => {
                let gp = self.group_priority(r);
                let line = gp < self.execution_priority(false, None);
                self.drive_irq(line);
                self.masked_present = gp < self.execution_priority(true, None);
                self.pending_exc = r as u16;
                Some(r as u16)
            }
        }
    }

    // --- pending / enable manipulation ----------------------------------------------

    fn should_escalate(&self, exc: usize, synchronous: bool) -> bool {
        let disabled = (EXC_MEMMANAGE..=EXC_USAGEFAULT).contains(&exc) && !self.fault_enable[exc - EXC_MEMMANAGE];
        disabled || (synchronous && !self.can_sync_become_active(exc, None))
    }

    pub fn can_sync_become_active(&self, exc: usize, ignored: Option<u16>) -> bool {
        let active = self.active.get(exc) && ignored != Some(exc as u16);
        // IsCandidate(state | Pending): enabled and not active.
        if !self.enabled.get(exc) || active {
            return false;
        }
        self.group_priority(exc) < self.execution_priority(false, ignored)
    }

    fn set_pending_no_escalation(&mut self, exc: usize) {
        let before = self.pending.get(exc);
        self.pending.set(exc);
        // SEVONPEND: any exception (even masked) entering the pending state generates an event.
        if !before && self.sevonpend {
            self.sev_pending_event = true;
        }
    }

    /// `SetPending`: pends `exc`, escalating disabled configurable faults to HardFault.
    pub fn set_pending(&mut self, exc: usize) {
        let mut e = exc;
        if self.should_escalate(exc, false) {
            self.hardfault_forced = true;
            e = EXC_HARDFAULT;
        }
        self.set_pending_no_escalation(e);
    }

    /// `SetPendingIRQ`: set pending followed by `FindPendingInterrupt`.
    pub fn set_pending_irq(&mut self, exc: usize) {
        self.set_pending(exc);
        self.find_pending();
    }

    /// `ClearPending`: ignored while the external line is still asserted.
    pub fn clear_pending(&mut self, exc: usize) {
        if !self.running.get(exc) {
            self.pending.clear(exc);
        }
    }

    pub fn set_enabled(&mut self, exc: usize, enabled: bool) {
        if enabled {
            self.enabled.set(exc);
        } else {
            self.enabled.clear(exc);
        }
    }

    /// Sets/clears the Active flag without touching the stack (SHCSR writes).
    pub fn force_active(&mut self, exc: usize, active: bool) {
        if active {
            self.active.set(exc);
        } else {
            self.active.clear(exc);
        }
    }

    /// External interrupt line `irq` (0-based) changed level (Renode `OnGPIO`).
    /// Returns true when the new level is high and an exception is selected
    /// (callers use it to wake a SysTick halted by deep sleep).
    pub fn set_irq_line(&mut self, irq: u32, level: bool) -> bool {
        if irq >= self.num_irqs {
            return false;
        }
        let exc = EXC_IRQ0 + irq as usize;
        if level {
            self.running.set(exc);
            self.set_pending(exc);
        } else {
            self.running.clear(exc);
        }
        let p = self.find_pending();
        p.is_some() && level
    }

    /// Raises a synchronous fault; escalates or reports lockup per Renode `SetPendingSynchronousFault`.
    pub fn set_pending_synchronous_fault(&mut self, exc: usize) -> SyncFault {
        if !self.should_escalate(exc, true) {
            self.set_pending_no_escalation(exc);
            self.find_pending();
            return SyncFault::Pending;
        }
        if !self.can_sync_become_active(EXC_HARDFAULT, None) {
            return SyncFault::Lockup;
        }
        self.hardfault_forced = true;
        self.set_pending_no_escalation(EXC_HARDFAULT);
        self.find_pending();
        SyncFault::Pending
    }

    /// `AcknowledgeIRQ`: marks the selected exception active and pushes it on the stack.
    pub fn acknowledge(&mut self) -> Option<u16> {
        let p = self.find_pending();
        if let Some(r) = p {
            let r = r as usize;
            self.active.set(r);
            self.pending.clear(r);
            self.active_stack.push(r as u16);
        }
        // At this point the interrupt can surely be deactivated, because the best one was chosen.
        self.drive_irq(false);
        p
    }

    /// `CompleteIRQ` for exception return. Returns false when `exc` is not the
    /// active top of stack (invalid return -> INVPC).
    pub fn complete(&mut self, exc: usize) -> bool {
        let is_active = self.active.get(exc);
        let is_top = self.active_stack.last().copied() == Some(exc as u16);
        if !is_active || !is_top {
            // ValidateExceptionReturn still deactivates the active fixed-priority exception.
            if let Some(fixed) = self.active_fixed_priority_exception() {
                self.deactivate(fixed);
                self.find_pending();
            }
            return false;
        }
        self.deactivate(exc);
        self.find_pending();
        true
    }

    fn active_fixed_priority_exception(&self) -> Option<usize> {
        match self.raw_execution_priority(None) {
            -2 => Some(EXC_NMI),
            -1 => Some(EXC_HARDFAULT),
            _ => None,
        }
    }

    fn deactivate(&mut self, exc: usize) {
        if self.active_stack.last().copied() == Some(exc as u16) {
            self.active_stack.pop();
        } else if let Some(pos) = self.active_stack.iter().rposition(|&e| e as usize == exc) {
            self.active_stack.remove(pos);
        }
        self.active.clear(exc);
        // Level-sensitive: a line that is still high re-pends the interrupt.
        if self.running.get(exc) {
            self.pending.set(exc);
        }
    }

    /// Enters lockup: only NMI can be taken until the state is cleared.
    pub fn set_lockup(&mut self, v: bool) {
        self.locked_up = v;
        if v {
            self.drive_irq(false);
        }
        self.find_pending();
    }

    /// `FPCCR` readiness snapshot (HFRDY bit 4, MMRDY 5, BFRDY 6, UFRDY 10) with the
    /// acknowledged `original_exception` hidden from the priority query.
    pub fn fpccr_ready_bits(&self, original_exception: u16) -> u32 {
        let ignored = if self.active_stack.last().copied() == Some(original_exception) && original_exception != 0 {
            Some(original_exception)
        } else {
            None
        };
        let ready = |exc: usize| -> bool {
            let en = !(EXC_MEMMANAGE..=EXC_USAGEFAULT).contains(&exc) || self.fault_enable[exc - EXC_MEMMANAGE];
            en && self.can_sync_become_active(exc, ignored)
        };
        let mut r = 0u32;
        if self.execution_priority(false, ignored) > -1 {
            r |= 1 << 4;
        }
        if ready(EXC_MEMMANAGE) {
            r |= 1 << 5;
        }
        if ready(EXC_BUSFAULT) {
            r |= 1 << 6;
        }
        if ready(EXC_USAGEFAULT) {
            r |= 1 << 10;
        }
        r
    }

    /// Debug summary line.
    pub fn summary(&self) -> String {
        format!(
            "active={:?} pending_exc={} irq_line={} primask={} faultmask={} basepri=0x{:02x} prigroup={} locked_up={}",
            self.active_stack, self.pending_exc, self.irq_line, self.primask as u8, self.faultmask as u8, self.basepri_raw, self.prigroup, self.locked_up
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvic() -> Nvic {
        Nvic::new(96, 0xF0)
    }

    #[test]
    fn external_irq_pends_enabled_only() {
        let mut n = nvic();
        n.set_irq_line(5, true);
        // Not enabled -> pending but not a candidate.
        assert_eq!(n.find_pending(), None);
        assert!(!n.irq_line);
        n.set_enabled(EXC_IRQ0 + 5, true);
        assert_eq!(n.find_pending(), Some(21));
        assert!(n.irq_line);
        assert_eq!(n.acknowledge(), Some(21));
        assert_eq!(n.current_exception(), 21);
        // Line still high: completion re-pends it.
        assert!(n.complete(21));
        assert!(n.is_pending(21));
        assert_eq!(n.find_pending(), Some(21));
        // Dropping the line stops the re-pending (the pending bit itself stays until taken).
        n.set_irq_line(5, false);
        assert_eq!(n.acknowledge(), Some(21));
        assert!(n.complete(21));
        assert!(!n.is_pending(21));
    }

    #[test]
    fn icpr_ignored_while_line_high() {
        let mut n = nvic();
        n.set_enabled(EXC_IRQ0 + 3, true);
        n.set_irq_line(3, true);
        n.clear_pending(EXC_IRQ0 + 3);
        assert!(n.is_pending(EXC_IRQ0 + 3));
        n.set_irq_line(3, false);
        n.clear_pending(EXC_IRQ0 + 3);
        assert!(!n.is_pending(EXC_IRQ0 + 3));
    }

    #[test]
    fn priority_and_preemption() {
        let mut n = nvic();
        n.set_enabled(EXC_IRQ0 + 1, true);
        n.set_enabled(EXC_IRQ0 + 2, true);
        n.set_priority(EXC_IRQ0 + 1, 0x80);
        n.set_priority(EXC_IRQ0 + 2, 0x40);
        n.set_pending_irq(EXC_IRQ0 + 1);
        n.set_pending_irq(EXC_IRQ0 + 2);
        // Higher priority (lower value) first.
        assert_eq!(n.acknowledge(), Some(18));
        // IRQ1 (0x80) cannot preempt IRQ2 (0x40).
        assert_eq!(n.find_pending(), None);
        assert!(!n.irq_line);
        assert!(n.complete(18));
        assert_eq!(n.find_pending(), Some(17));
        assert!(n.irq_line);
        assert_eq!(n.acknowledge(), Some(17));
        // A higher priority IRQ preempts IRQ1.
        n.set_pending_irq(EXC_IRQ0 + 2);
        assert!(n.irq_line);
        assert_eq!(n.acknowledge(), Some(18));
        assert_eq!(n.active_stack(), &[17, 18]);
        assert!(n.complete(18));
        assert!(n.complete(17));
        assert!(n.active_stack().is_empty());
    }

    #[test]
    fn prigroup_blocks_same_group() {
        let mut n = nvic();
        // PRIGROUP=3 -> group bits [7:4], sub-priority [3:0]; with 0xF0 mask all priorities are group priorities.
        n.prigroup = 3;
        n.set_enabled(EXC_IRQ0, true);
        n.set_enabled(EXC_IRQ0 + 1, true);
        n.set_priority(EXC_IRQ0, 0x40);
        n.set_priority(EXC_IRQ0 + 1, 0x40);
        n.set_pending_irq(EXC_IRQ0);
        assert_eq!(n.acknowledge(), Some(16));
        n.set_pending_irq(EXC_IRQ0 + 1);
        // Same group priority -> no preemption.
        assert!(!n.irq_line);
        // With PRIGROUP=7 the whole byte is sub-priority: nothing preempts anything.
        let mut m = nvic();
        m.prigroup = 7;
        m.set_enabled(EXC_IRQ0, true);
        m.set_enabled(EXC_IRQ0 + 1, true);
        m.set_priority(EXC_IRQ0, 0x80);
        m.set_priority(EXC_IRQ0 + 1, 0x10);
        m.set_pending_irq(EXC_IRQ0);
        assert_eq!(m.acknowledge(), Some(16));
        m.set_pending_irq(EXC_IRQ0 + 1);
        assert!(!m.irq_line);
    }

    #[test]
    fn primask_basepri_faultmask() {
        let mut n = nvic();
        n.set_enabled(EXC_IRQ0, true);
        n.set_priority(EXC_IRQ0, 0x60);
        n.set_pending_irq(EXC_IRQ0);
        assert!(n.irq_line && n.masked_present);
        n.primask = true;
        n.find_pending();
        assert!(!n.irq_line);
        assert!(n.masked_present); // WFI would still wake
        n.primask = false;
        n.set_basepri(0x60); // blocks priorities >= 0x60
        n.find_pending();
        assert!(!n.irq_line);
        assert!(!n.masked_present);
        n.set_basepri(0x70);
        n.find_pending();
        assert!(n.irq_line);
        n.set_basepri(0);
        n.faultmask = true;
        n.find_pending();
        assert!(!n.irq_line);
        // NMI is not blocked by FAULTMASK? NMI priority -2 < -1.
        n.set_pending_irq(EXC_NMI);
        assert_eq!(n.pending_exc, 2);
        assert!(n.irq_line);
    }

    #[test]
    fn sync_fault_escalation_and_lockup() {
        let mut n = nvic();
        // UsageFault disabled -> escalates to HardFault with FORCED.
        assert_eq!(n.set_pending_synchronous_fault(EXC_USAGEFAULT), SyncFault::Pending);
        assert!(n.hardfault_forced);
        assert!(n.is_pending(EXC_HARDFAULT));
        assert!(!n.is_pending(EXC_USAGEFAULT));
        assert_eq!(n.acknowledge(), Some(3));
        // A fault inside the HardFault handler cannot be taken: lockup.
        assert_eq!(n.set_pending_synchronous_fault(EXC_USAGEFAULT), SyncFault::Lockup);
        assert!(n.complete(3));
        // With UsageFault enabled it is taken as UsageFault.
        n.fault_enable[2] = true;
        n.hardfault_forced = false;
        assert_eq!(n.set_pending_synchronous_fault(EXC_USAGEFAULT), SyncFault::Pending);
        assert!(!n.hardfault_forced);
        assert_eq!(n.acknowledge(), Some(6));
        // A second UsageFault while the handler is active escalates to HardFault (fits: HF can preempt).
        assert_eq!(n.set_pending_synchronous_fault(EXC_USAGEFAULT), SyncFault::Pending);
        assert!(n.hardfault_forced);
        assert_eq!(n.find_pending(), Some(3));
    }

    #[test]
    fn svc_priority_escalation() {
        let mut n = nvic();
        n.set_priority(EXC_SVCALL, 0x00);
        n.set_priority(EXC_PENDSV, 0xF0);
        n.set_pending_irq(EXC_PENDSV);
        assert_eq!(n.acknowledge(), Some(14));
        // SVC (priority 0) preempts PendSV (0xF0).
        assert_eq!(n.set_pending_synchronous_fault(EXC_SVCALL), SyncFault::Pending);
        assert!(!n.hardfault_forced);
        assert_eq!(n.find_pending(), Some(11));
        assert_eq!(n.acknowledge(), Some(11));
        // SVC inside the SVC handler cannot preempt itself -> HardFault.
        assert_eq!(n.set_pending_synchronous_fault(EXC_SVCALL), SyncFault::Pending);
        assert!(n.hardfault_forced);
    }

    #[test]
    fn register_views() {
        let mut n = nvic();
        n.set_enabled(EXC_IRQ0 + 31, true);
        n.set_enabled(EXC_IRQ0 + 32, true);
        assert_eq!(n.enabled_irq_reg(0), 0x8000_0000);
        assert_eq!(n.enabled_irq_reg(1), 0x0000_0001);
        n.set_irq_line(33, true);
        assert_eq!(n.pending_irq_reg(1), 0x0000_0002);
        assert_eq!(n.pending_irq_reg(0), 0);
    }

    #[test]
    fn sevonpend_event() {
        let mut n = nvic();
        n.sevonpend = true;
        n.set_pending(EXC_PENDSV);
        assert!(n.sev_pending_event);
    }
}
