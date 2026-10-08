// Ported from emulation/models/NGCParallelLCD.cs.
// The repaint thread and the visible-buffer handling follow Renode 1.17.0
// src/Emulator/Main/Peripherals/Video/AutoRepaintingVideo.cs (MIT License, Copyright (c) Antmicro, Realtime Embedded).

//! `NGCParallelLCD`: the handset's 240x320 RGB565 parallel LCD behind the FMC window (`lcd @ 0x60000000`,
//! size `0x20004`), with its TE output and PB4 reset input. A protocol-level approximation recovered from
//! the unmodified handset 65.3 driver, **not** a verified physical panel model (`emulation/lcd-protocol.md`).
//!
//! # Bus protocol
//!
//! Halfword (and, truncated to 16 bits, word) writes: offset `0` is the *command* port, offset `0x20002` the
//! *data* port. One command parameter byte occupies one halfword write and each RGB565 pixel one halfword
//! write. Reads return data only at the data port (display ID `79 85 52` after command `0x04` with one dummy
//! read, `0xDA/0xDB/0xDC` one ID byte each, otherwise 0). Supported commands: `0x01` software reset, `0x10/0x11`
//! sleep in/out, `0x28/0x29` display off/on, `0x34/0x35` TE off/on, `0x2A/0x2B` column/row window (4
//! parameters, inclusive ends), `0x2C/0x3C` memory write (cursor to the window origin, then pixels),
//! `0x36` MADCTL (bit 5 = axis swap: logical 320x240; bit 3 = BGR order), `0x3A` COLMOD (stored; anything but
//! RGB565 logs a warning). Every other command (vendor power/gamma/porch) is accepted and ignored. Physical
//! MY/MX flips, analog settings, backlight and scan timing are not simulated.
//!
//! # Frames, TE and the visible buffer
//!
//! Pixels land in a 76 800-entry GRAM; a *visible buffer* (Renode's `buffer`) mirrors it only while the display
//! is on and awake (`panelOn && !sleeping`), otherwise it is black. Renode refreshes it in the repaint thread
//! (`FramesPerVirtualSecond = 120`, a managed thread of period `ceil(1e9 / 120) = 8 333 334` ns) and in `SavePPM`;
//! each repaint with TE enabled and the display on also toggles the `TE` output (rising edges every
//! 16 666 668 ns, ~60 Hz) and counts `TEPulses`. The visible buffer is only rebuilt when the GRAM changed or the
//! visibility flipped (TE keeps toggling on unchanged frames).
//!
//! For the browser: [`NgcParallelLcd::frame_rgba`] is the visible buffer as RGBA8888 (`width()` x `height()`,
//! row-major, alpha 255) and [`NgcParallelLcd::frame_version`] changes **only when the visible pixels or the
//! geometry change**. [`NgcParallelLcd::sync_frame`] brings the visible buffer up to date (what `SavePPM` does
//! before writing, independent of the 120 Hz repaint) and returns the version; [`NgcParallelLcd::export_ppm`] is the
//! byte-identical equivalent of the Renode `SavePPM` image (`P6`, expansion `(c << 3) | (c >> 2)` for 5-bit and
//! `(g << 2) | (g >> 4)` for 6-bit components).
//!
//! # Wiring (`handset.repl`)
//!
//! ```text
//! lcd: Video.NGCParallelLCD @ sysbus 0x60000000     board.add_mapped(0x6000_0000, lcd::SIZE, NgcParallelLcd::new("lcd", true))
//!     simulateTE: true                              (the constructor argument)
//!     TE -> gpioD@3 | exti@3                        connect_input(lcd, lcd::TE, gpio_d, 3); connect_input(lcd, lcd::TE, exti, 3)
//! gpioB: 4 -> lcd@0                                 connect_input(gpio_b, 4, lcd, lcd::RESET_INPUT)
//! ```
//!
//! Connecting PB4 pushes its initial low level, which resets the (still empty) LCD, as in Renode. The repaint
//! thread is one managed thread created in `attach`, so register the LCD in `.repl` order relative to the other
//! clock entries.
//!
//! # Hot path
//!
//! A pixel write is the most frequent MMIO access of the emulator (about 600 000 per boot). The path in
//! `Peripheral::write` is: offset test, `pixel_mode` flag test, then [`NgcParallelLcd::write_pixel`], which does
//! a bounds test, an optional red/blue swap, one compare-and-store into the GRAM with a branchless non-black
//! counter update, and the cursor advance: no allocation, no formatting, no logging. Measured (opt-level 2,
//! Apple silicon): about 2.4 ns per write in the model, about 12 ns through the `emu-core` bus dispatch.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, LogLevel, ManagedThread, Peripheral, Time, Translations, View, Width, Widths};

/// `Size`: the register window (`DataOffset + 2`).
pub const SIZE: u32 = 0x20004;
/// Offset of the command port.
pub const COMMAND_OFFSET: u32 = 0;
/// Offset of the data port (`0x60020002` on the bus).
pub const DATA_OFFSET: u32 = 0x20002;
/// Output line 0: `TE` (`.repl`: `TE -> gpioD@3 | exti@3`).
pub const TE: u32 = 0;
/// Input line 0: active-low reset (`.repl`: `gpioB: 4 -> lcd@0`).
pub const RESET_INPUT: u32 = 0;
/// Pixels of the panel in either orientation.
pub const PIXELS: usize = 240 * 320;
/// `FramesPerVirtualSecond`: two half-periods per TE cycle.
pub const FRAMES_PER_VIRTUAL_SECOND: u64 = 120;

/// Token of the repaint thread.
const REPAINT: u64 = 1;
/// Parameter bytes buffered per command (`args = new byte[32]`).
const ARG_CAPACITY: usize = 32;

/// 5-bit to 8-bit component expansion `(c << 3) | (c >> 2)`.
const EXPAND5: [u8; 32] = {
    let mut table = [0u8; 32];
    let mut c = 0;
    while c < 32 {
        table[c] = ((c << 3) | (c >> 2)) as u8;
        c += 1;
    }
    table
};

/// 6-bit to 8-bit component expansion `(c << 2) | (c >> 4)`.
const EXPAND6: [u8; 64] = {
    let mut table = [0u8; 64];
    let mut c = 0;
    while c < 64 {
        table[c] = ((c << 2) | (c >> 4)) as u8;
        c += 1;
    }
    table
};

/// C# `bool.ToString()`.
fn cs_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// `Video.NGCParallelLCD` (with the relevant parts of `AutoRepaintingVideo`).
pub struct NgcParallelLcd {
    name: String,
    simulate_te: bool,
    repaint: ManagedThread,
    /// 240x320 GRAM in the current orientation's row-major order.
    gram: Box<[u16]>,
    /// Renode's `buffer`: the visible RGB565 pixels (black while the display is off or asleep).
    visible: Box<[u16]>,
    /// RGBA8888 conversion of `visible`, valid for `rgba_version`.
    rgba: Vec<u8>,
    rgba_version: u64,
    /// Changes whenever the visible pixels or the geometry change.
    version: u64,
    /// `visible` is known to be all zero.
    visible_black: bool,
    // Geometry (`Width`/`Height`).
    width: u32,
    height: u32,
    // Protocol state.
    command: u8,
    /// Cached `command == 0x2C || command == 0x3C`: data writes are pixels.
    pixel_mode: bool,
    madctl: u8,
    /// Cached `madctl & 8`: red and blue swapped on write.
    bgr: bool,
    color_mode: u8,
    arg_count: usize,
    args: [u8; ARG_CAPACITY],
    read_index: i32,
    column_start: i32,
    column_end: i32,
    row_start: i32,
    row_end: i32,
    cursor_x: i32,
    cursor_y: i32,
    sleeping: bool,
    display_on: bool,
    te_enabled: bool,
    te_high: bool,
    visible_dirty: bool,
    visible_enabled: bool,
    non_black: i32,
    command_writes: u64,
    data_writes: u64,
    pixel_writes: u64,
    read_count: u64,
    te_pulses: u64,
}

impl NgcParallelLcd {
    /// `new NGCParallelLCD(machine, simulateTE = true)` named `name` (`lcd` in `handset.repl`, `simulateTE: true`).
    pub fn new(name: impl Into<String>, simulate_te: bool) -> Self {
        let mut lcd = Self {
            name: name.into(),
            simulate_te,
            repaint: ManagedThread::new(FRAMES_PER_VIRTUAL_SECOND, REPAINT),
            gram: vec![0u16; PIXELS].into_boxed_slice(),
            visible: vec![0u16; PIXELS].into_boxed_slice(),
            rgba: vec![0u8; PIXELS * 4],
            rgba_version: u64::MAX,
            version: 1,
            visible_black: true,
            width: 240,
            height: 320,
            command: 0,
            pixel_mode: false,
            madctl: 0,
            bgr: false,
            color_mode: 5,
            arg_count: 0,
            args: [0; ARG_CAPACITY],
            read_index: 0,
            column_start: 0,
            column_end: 239,
            row_start: 0,
            row_end: 319,
            cursor_x: 0,
            cursor_y: 0,
            sleeping: true,
            display_on: false,
            te_enabled: false,
            te_high: false,
            visible_dirty: false,
            visible_enabled: false,
            non_black: 0,
            command_writes: 0,
            data_writes: 0,
            pixel_writes: 0,
            read_count: 0,
            te_pulses: 0,
        };
        lcd.reset_core();
        lcd
    }

    /// `Reset()` without the output line and the repaint thread. Returns true when the C# `Reset` would call
    /// `Reconfigure` (the geometry was not 240x320), which restarts the repaint thread.
    fn reset_core(&mut self) -> bool {
        // Renode parity: a reset (command 0x01, PB4 low, machine reset) also zeroes every counter and `TEPulses`.
        self.command = 0;
        self.pixel_mode = false;
        self.arg_count = 0;
        self.read_index = 0;
        self.madctl = 0;
        self.bgr = false;
        self.color_mode = 5;
        self.sleeping = true;
        self.display_on = false;
        self.te_enabled = false;
        self.te_high = false;
        self.column_start = 0;
        self.column_end = 239;
        self.row_start = 0;
        self.row_end = 319;
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.gram.fill(0);
        let reconfigured = self.width != 240 || self.height != 320;
        if reconfigured {
            self.reconfigure(240, 320);
        }
        // `Array.Clear(buffer, 0, buffer.Length)`.
        if !self.visible_black {
            self.visible.fill(0);
            self.visible_black = true;
            self.version += 1;
        }
        self.non_black = 0;
        self.visible_dirty = false;
        self.visible_enabled = false;
        self.command_writes = 0;
        self.data_writes = 0;
        self.pixel_writes = 0;
        self.read_count = 0;
        self.te_pulses = 0;
        reconfigured
    }

    /// `Reconfigure(width, height, RGB565)`: a new zeroed buffer; the repaint thread is (re)started by the caller.
    fn reconfigure(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.visible.fill(0);
        self.visible_black = true;
        self.version += 1;
    }

    // ---- C# properties and runner methods ----

    /// `Width` of the current (logical) orientation: 240 or 320.
    pub fn width(&self) -> usize {
        self.width as usize
    }

    /// `Height` of the current (logical) orientation: 320 or 240.
    pub fn height(&self) -> usize {
        self.height as usize
    }

    /// `CommandWrites`.
    pub fn command_writes(&self) -> u64 {
        self.command_writes
    }

    /// `DataWrites`: every data-port write (pixels and command parameters).
    pub fn data_writes(&self) -> u64 {
        self.data_writes
    }

    /// `PixelWrites`: pixels written after a memory-write command.
    pub fn pixel_writes(&self) -> u64 {
        self.pixel_writes
    }

    /// `ReadCount`: every read of the window.
    pub fn read_count(&self) -> u64 {
        self.read_count
    }

    /// `TEPulses`: rising TE edges.
    pub fn te_pulses(&self) -> u64 {
        self.te_pulses
    }

    /// `DisplayEnabled`: display on and not sleeping.
    pub fn display_enabled(&self) -> bool {
        self.display_on && !self.sleeping
    }

    /// `panelOn` of the summary.
    pub fn panel_on(&self) -> bool {
        self.display_on
    }

    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    /// Last MADCTL parameter.
    pub fn madctl(&self) -> u8 {
        self.madctl
    }

    /// Last COLMOD parameter.
    pub fn color_mode(&self) -> u8 {
        self.color_mode
    }

    /// `nonBlackPixels`: GRAM pixels that are not zero.
    pub fn non_black_pixels(&self) -> i32 {
        self.non_black
    }

    /// The GRAM pixel (RGB565 value as stored, after the optional BGR swap) at logical `(x, y)`.
    pub fn gram_pixel(&self, x: usize, y: usize) -> Option<u16> {
        (x < self.width as usize && y < self.height as usize).then(|| self.gram[y * self.width as usize + x])
    }

    /// Current cursor, window as `(column_start, column_end, row_start, row_end)` (diagnostics).
    pub fn cursor(&self) -> (i32, i32) {
        (self.cursor_x, self.cursor_y)
    }

    pub fn window(&self) -> (i32, i32, i32, i32) {
        (self.column_start, self.column_end, self.row_start, self.row_end)
    }

    /// C# `Summary`.
    pub fn describe(&self) -> String {
        format!(
            "{}x{}; panelOn={}; sleeping={}; MADCTL=0x{:02X}; COLMOD=0x{:02X}; commands={}; data={}; pixels={}; nonBlackGRAM={}; reads={}; TE={}",
            self.width,
            self.height,
            cs_bool(self.display_on),
            cs_bool(self.sleeping),
            self.madctl,
            self.color_mode,
            self.command_writes,
            self.data_writes,
            self.pixel_writes,
            self.non_black,
            self.read_count,
            self.te_pulses
        )
    }

    // ---- frames ----

    /// `RefreshVisibleBuffer()`: rebuilds the visible buffer from the GRAM (or blanks it) when the GRAM changed
    /// or the visibility flipped; otherwise a no-op. Bumps the frame version only if the visible pixels change.
    pub fn refresh_visible_buffer(&mut self) {
        let visible = self.display_on && !self.sleeping;
        if !self.visible_dirty && self.visible_enabled == visible {
            return;
        }
        if !visible {
            if !self.visible_black {
                self.visible.fill(0);
                self.visible_black = true;
                self.version += 1;
            }
        } else {
            if self.visible[..] != self.gram[..] {
                self.visible.copy_from_slice(&self.gram);
                self.version += 1;
            }
            self.visible_black = self.non_black == 0;
        }
        self.visible_dirty = false;
        self.visible_enabled = visible;
    }

    /// Brings the visible buffer up to date (as `SavePPM` does before writing) and returns the frame version.
    pub fn sync_frame(&mut self) -> u64 {
        self.refresh_visible_buffer();
        self.version
    }

    /// The frame version as of the last refresh (repaint tick, [`sync_frame`](Self::sync_frame) or export):
    /// changes only when the visible pixels or the geometry change.
    pub fn frame_version(&self) -> u64 {
        self.version
    }

    /// The visible buffer as RGB565 values (`width() * height()` entries used), as of the last refresh. Its content
    /// between two repaint ticks depends on whether the host called [`sync_frame`](Self::sync_frame) or
    /// [`export_ppm`](Self::export_ppm) in between; use [`gram_rgb565`](Self::gram_rgb565) for state hashes.
    pub fn visible_rgb565(&self) -> &[u16] {
        &self.visible[..self.width as usize * self.height as usize]
    }

    /// The GRAM (`width() * height()` entries used): pure emulated state, independent of any host-side refresh.
    pub fn gram_rgb565(&self) -> &[u16] {
        &self.gram[..self.width as usize * self.height as usize]
    }

    /// The visible buffer as RGBA8888 (R, G, B, 255 per pixel, row-major, `width() * height() * 4` bytes), converted
    /// lazily and only when [`frame_version`](Self::frame_version) changed since the last conversion. It does not
    /// refresh the visible buffer: call [`sync_frame`](Self::sync_frame) first to include the latest GRAM writes.
    pub fn frame_rgba(&mut self) -> &[u8] {
        let count = self.width as usize * self.height as usize;
        if self.rgba_version != self.version {
            for (out, &pixel) in self.rgba.chunks_exact_mut(4).zip(self.visible[..count].iter()) {
                let pixel = u32::from(pixel);
                out[0] = EXPAND5[((pixel >> 11) & 31) as usize];
                out[1] = EXPAND6[((pixel >> 5) & 63) as usize];
                out[2] = EXPAND5[(pixel & 31) as usize];
                out[3] = 255;
            }
            self.rgba_version = self.version;
        }
        &self.rgba[..count * 4]
    }

    /// The Renode `lcd SavePPM` image: refreshes the visible buffer, then `P6\n<w> <h>\n255\n` and the RGB triples.
    pub fn export_ppm(&mut self) -> Vec<u8> {
        self.refresh_visible_buffer();
        let count = self.width as usize * self.height as usize;
        let mut out = Vec::with_capacity(24 + count * 3);
        out.extend_from_slice(format!("P6\n{} {}\n255\n", self.width, self.height).as_bytes());
        for &pixel in &self.visible[..count] {
            let pixel = u32::from(pixel);
            out.push(EXPAND5[((pixel >> 11) & 31) as usize]);
            out.push(EXPAND6[((pixel >> 5) & 63) as usize]);
            out.push(EXPAND5[(pixel & 31) as usize]);
        }
        out
    }

    // ---- protocol ----

    /// The hot path: one pixel to the GRAM at the cursor, then the cursor advance inside the window.
    #[inline(always)]
    fn write_pixel(&mut self, pixel: u16) {
        let x = self.cursor_x;
        let y = self.cursor_y;
        if (x as u32) < self.width && (y as u32) < self.height {
            // Frames are shown in logical MCU addressing orientation. Physical MY/MX mounting flips and scan
            // timing are deliberately abstracted.
            let pixel = if self.bgr { (pixel & 0x07E0) | ((pixel & 31) << 11) | ((pixel >> 11) & 31) } else { pixel };
            let index = (y as u32 * self.width + x as u32) as usize;
            // x < width and y < height with width * height == PIXELS in both orientations (measured: the bounds
            // check on `index` costs nothing next to the compare-and-store; no unsafe indexing needed).
            let cell = &mut self.gram[index];
            let old = *cell;
            if old != pixel {
                self.non_black += i32::from(pixel != 0) - i32::from(old != 0);
                *cell = pixel;
                self.visible_dirty = true;
            }
        }
        self.pixel_writes += 1;
        self.cursor_x += 1;
        if self.cursor_x > self.column_end {
            self.cursor_x = self.column_start;
            self.cursor_y += 1;
            if self.cursor_y > self.row_end {
                self.cursor_y = self.row_start;
            }
        }
    }

    /// Data write that is not a pixel: a command parameter byte.
    #[inline(never)]
    fn write_argument(&mut self, value: u16, ctx: &mut Ctx<'_>) {
        if self.arg_count < ARG_CAPACITY {
            // Commands carry one byte in each physical 16-bit bus transaction.
            self.args[self.arg_count] = value as u8;
            self.arg_count += 1;
        }
        let args = &self.args;
        if self.command == 0x2A && self.arg_count == 4 {
            self.column_start = i32::from(args[0]) * 256 + i32::from(args[1]);
            self.column_end = i32::from(args[2]) * 256 + i32::from(args[3]);
        } else if self.command == 0x2B && self.arg_count == 4 {
            self.row_start = i32::from(args[0]) * 256 + i32::from(args[1]);
            self.row_end = i32::from(args[2]) * 256 + i32::from(args[3]);
        } else if self.command == 0x36 && self.arg_count == 1 {
            self.madctl = args[0];
            self.bgr = self.madctl & 8 != 0;
            let landscape = self.madctl & 0x20 != 0;
            let (width, height) = if landscape { (320, 240) } else { (240, 320) };
            if self.width != width || self.height != height {
                self.gram.fill(0);
                self.non_black = 0;
                self.visible_dirty = true;
                self.reconfigure(width, height);
                self.repaint.start(ctx);
            }
        } else if self.command == 0x3A && self.arg_count == 1 {
            self.color_mode = args[0];
            if self.color_mode & 7 != 5 {
                let mode = self.color_mode;
                ctx.logf(LogLevel::Warning, format_args!("LCD color mode 0x{mode:02X} is not recovered RGB565"));
            }
        }
        // Vendor power/gamma/porch commands from 0x08008610 are accepted as no-ops. Their analog electrical
        // effects are outside this model.
    }

    /// Command-port write (offset 0) or an unexpected offset.
    #[inline(never)]
    fn write_command_port(&mut self, offset: u32, value: u16, ctx: &mut Ctx<'_>) {
        if offset != COMMAND_OFFSET {
            ctx.warn_once(u64::from(offset), format_args!("Unexpected LCD write offset 0x{offset:X}"));
            return;
        }
        self.command_writes += 1;
        self.command = value as u8;
        self.pixel_mode = self.command == 0x2C || self.command == 0x3C;
        self.arg_count = 0;
        self.read_index = 0;
        let command = self.command;
        ctx.logf(LogLevel::Debug, format_args!("LCD command 0x{command:02X}"));
        match command {
            0x01 => self.software_reset(ctx),
            0x10 => self.sleeping = true,
            0x11 => self.sleeping = false,
            0x28 => self.display_on = false,
            0x29 => self.display_on = true,
            0x34 => {
                self.te_enabled = false;
                ctx.set_output(TE, false);
                self.te_high = false;
            }
            0x35 => self.te_enabled = true,
            0x2C | 0x3C => {
                // Renode parity: memory-write-continue (0x3C) restarts at the window origin like RAMWR.
                self.cursor_x = self.column_start;
                self.cursor_y = self.row_start;
            }
            _ => {}
        }
    }

    /// `Reset()` as called by command `0x01`, the reset input and the machine: TE low and the repaint thread
    /// (re)started when the geometry changed.
    fn software_reset(&mut self, ctx: &mut Ctx<'_>) {
        let reconfigured = self.reset_core();
        ctx.set_output(TE, false);
        if reconfigured {
            self.repaint.start(ctx);
        }
    }

    /// Display ID and the data port reads.
    fn read_data_port(&mut self) -> u32 {
        // 0x0800725c gates initialization on ID 0x798552; 0x080085ce consumes one dummy read before the three
        // returned bytes.
        match self.command {
            0x04 => {
                let index = self.read_index;
                self.read_index = self.read_index.wrapping_add(1);
                match index {
                    1 => 0x79,
                    2 => 0x85,
                    3 => 0x52,
                    _ => 0,
                }
            }
            0xDA | 0xDB | 0xDC => {
                let first = self.read_index == 0;
                self.read_index = self.read_index.wrapping_add(1);
                if first {
                    0
                } else {
                    match self.command {
                        0xDA => 0x79,
                        0xDB => 0x85,
                        _ => 0x52,
                    }
                }
            }
            _ => 0,
        }
    }

    /// One repaint tick (`DoRepaint` -> `Repaint`).
    fn repaint_tick(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh_visible_buffer();
        if self.simulate_te && self.te_enabled && self.display_on && !self.sleeping {
            self.te_high = !self.te_high;
            ctx.set_output(TE, self.te_high);
            if self.te_high {
                self.te_pulses += 1;
            }
        } else {
            self.te_high = false;
            ctx.set_output(TE, false);
        }
    }
}

impl Peripheral for NgcParallelLcd {
    fn name(&self) -> &str {
        &self.name
    }

    /// The repaint thread (the `AutoRepaintingVideo` managed thread, 120 Hz) runs from the moment the
    /// peripheral exists: the first TE toggle is `ceil(1e9 / 120)` ns after time 0.
    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.repaint.attach(ctx);
        self.repaint.start(ctx);
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.software_reset(ctx);
    }

    fn read(&mut self, offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        self.read_count += 1;
        if offset != DATA_OFFSET {
            return 0;
        }
        self.read_data_port()
    }

    #[inline]
    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        // `WriteDoubleWord` forwards the low 16 bits to `WriteWord`.
        let value = value as u16;
        if offset == DATA_OFFSET {
            self.data_writes += 1;
            if self.pixel_mode {
                self.write_pixel(value);
            } else {
                self.write_argument(value, ctx);
            }
        } else {
            self.write_command_port(offset, value, ctx);
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        if token == REPAINT {
            self.repaint_tick(ctx);
        }
    }

    /// Input 0 is PB4 active-low reset. The actual backlight circuit is not modeled.
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        if line == RESET_INPUT && !level {
            self.software_reset(ctx);
        }
    }

    // IWordPeripheral + IDoubleWordPeripheral, no [AllowedTranslations]: byte accesses are not supported.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::new(Widths::HALF | Widths::WORD, Translations::NONE)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.describe()
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{PeriphId, TICKS_PER_MILLISECOND as MS};

    const BASE: u32 = 0x6000_0000;
    const DATA: u32 = BASE + DATA_OFFSET;
    const PERIOD: Time = 8_333_334;

    fn setup() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, NgcParallelLcd::new("lcd", true));
        (h, id)
    }

    fn lcd(h: &Harness, id: PeriphId) -> &NgcParallelLcd {
        h.get::<NgcParallelLcd>(id)
    }

    fn command(h: &mut Harness, command: u32) {
        h.write16(BASE, command);
    }

    fn data(h: &mut Harness, value: u32) {
        h.write16(DATA, value);
    }

    fn window(h: &mut Harness, x0: u32, x1: u32, y0: u32, y1: u32) {
        command(h, 0x2A);
        for v in [x0 >> 8, x0 & 255, x1 >> 8, x1 & 255] {
            data(h, v);
        }
        command(h, 0x2B);
        for v in [y0 >> 8, y0 & 255, y1 >> 8, y1 & 255] {
            data(h, v);
        }
    }

    /// The driver's bring-up: sleep out, COLMOD, TE on, display on.
    fn bring_up(h: &mut Harness) {
        command(h, 0x11);
        command(h, 0x3A);
        data(h, 0x05);
        command(h, 0x35);
        data(h, 0x00);
        command(h, 0x29);
    }

    fn landscape(h: &mut Harness) {
        command(h, 0x36);
        data(h, 0x60);
    }

    #[test]
    fn reset_state_and_summary() {
        let (h, id) = setup();
        assert_eq!(
            lcd(&h, id).describe(),
            "240x320; panelOn=False; sleeping=True; MADCTL=0x00; COLMOD=0x05; commands=0; data=0; pixels=0; nonBlackGRAM=0; reads=0; TE=0"
        );
        assert_eq!(h.core().summaries().iter().find(|(n, _)| n == "lcd").unwrap().1, lcd(&h, id).describe());
        assert_eq!((lcd(&h, id).width(), lcd(&h, id).height()), (240, 320));
        assert_eq!(lcd(&h, id).window(), (0, 239, 0, 319));
        assert!(!lcd(&h, id).display_enabled());
        assert_eq!(h.core().clock_entry_count(), 1, "one managed thread (the 120 Hz repainter)");
    }

    #[test]
    fn display_id_reads() {
        let (mut h, id) = setup();
        assert_eq!(h.read16(DATA), 0, "no command yet");
        command(&mut h, 0x04);
        assert_eq!([h.read16(DATA), h.read16(DATA), h.read16(DATA), h.read16(DATA), h.read16(DATA)], [0, 0x79, 0x85, 0x52, 0]);
        // A new command restarts the read index.
        command(&mut h, 0x04);
        assert_eq!(h.read32(DATA), 0, "word reads return the same value, zero-extended");
        assert_eq!(h.read32(DATA), 0x79);
        for (cmd, value) in [(0xDA, 0x79), (0xDB, 0x85), (0xDC, 0x52)] {
            command(&mut h, cmd);
            assert_eq!([h.read16(DATA), h.read16(DATA), h.read16(DATA)], [0, value, value], "command 0x{cmd:X}");
        }
        command(&mut h, 0x09);
        assert_eq!(h.read16(DATA), 0);
        assert_eq!(h.read16(BASE), 0, "the command port reads zero but is counted");
        assert_eq!(h.read16(BASE + 0x100), 0);
        assert_eq!(lcd(&h, id).read_count(), 1 + 5 + 2 + 9 + 1 + 1 + 1);
        assert_eq!(h.read8(DATA), 0, "byte accesses are rejected by the bus");
        assert_eq!(lcd(&h, id).read_count(), 20, "and not counted");
        assert_eq!(h.warnings().len(), 1);
    }

    #[test]
    fn windows_cursor_and_wrap() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        window(&mut h, 1, 2, 3, 4);
        assert_eq!(lcd(&h, id).window(), (1, 2, 3, 4));
        command(&mut h, 0x2C);
        assert_eq!(lcd(&h, id).cursor(), (1, 3));
        for pixel in 1..=5u32 {
            data(&mut h, pixel);
        }
        assert_eq!(lcd(&h, id).gram_rgb565()[3 * 240 + 1], 5, "the GRAM view is the emulated state");
        assert_eq!(lcd(&h, id).gram_rgb565().len(), 240 * 320);
        let l = lcd(&h, id);
        assert_eq!(
            [l.gram_pixel(1, 3), l.gram_pixel(2, 3), l.gram_pixel(1, 4), l.gram_pixel(2, 4), l.gram_pixel(1, 3)],
            [Some(5), Some(2), Some(3), Some(4), Some(5)],
            "the fifth pixel wraps the 2x2 window back to its origin"
        );
        assert_eq!(l.cursor(), (2, 3));
        assert_eq!(l.pixel_writes(), 5);
        assert_eq!(l.non_black_pixels(), 4);
        // Memory write continue (0x3C) also restarts at the window origin.
        command(&mut h, 0x3C);
        assert_eq!(lcd(&h, id).cursor(), (1, 3));
        data(&mut h, 0);
        assert_eq!(lcd(&h, id).non_black_pixels(), 3, "overwriting with black decrements the counter");
        // A write of the same value changes nothing (and counts as a pixel).
        data(&mut h, 2);
        assert_eq!(lcd(&h, id).non_black_pixels(), 3);
        assert_eq!(lcd(&h, id).pixel_writes(), 7);
    }

    #[test]
    fn cursor_outside_the_panel_skips_the_store_but_advances() {
        let (mut h, id) = setup();
        window(&mut h, 238, 241, 319, 320);
        command(&mut h, 0x2C);
        for pixel in 1..=8u32 {
            data(&mut h, pixel);
        }
        let l = lcd(&h, id);
        assert_eq!(l.pixel_writes(), 8);
        assert_eq!([l.gram_pixel(238, 319), l.gram_pixel(239, 319)], [Some(1), Some(2)]);
        assert_eq!(l.non_black_pixels(), 2, "x = 240, 241 and y = 320 are outside the 240x320 GRAM");
        assert_eq!(l.cursor(), (238, 319), "after eight pixels the 4x2 window wrapped to its origin");
        // Windows can exceed the 16-bit parameter range semantics: 0xFFFF end is legal.
        window(&mut h, 0, 0xFFFF, 0, 0xFFFF);
        assert_eq!(lcd(&h, id).window(), (0, 65535, 0, 65535));
    }

    #[test]
    fn bgr_flag_swaps_red_and_blue_on_write() {
        let (mut h, id) = setup();
        command(&mut h, 0x36);
        data(&mut h, 0x08);
        window(&mut h, 0, 2, 0, 0);
        command(&mut h, 0x2C);
        data(&mut h, 0xF800); // red becomes blue
        data(&mut h, 0x001F); // blue becomes red
        data(&mut h, 0x07E0); // green is untouched
        let l = lcd(&h, id);
        assert_eq!([l.gram_pixel(0, 0), l.gram_pixel(1, 0), l.gram_pixel(2, 0)], [Some(0x001F), Some(0xF800), Some(0x07E0)]);
        assert_eq!(l.madctl(), 0x08);
        assert_eq!((l.width(), l.height()), (240, 320), "BGR does not rotate");
        command(&mut h, 0x36);
        data(&mut h, 0x00);
        command(&mut h, 0x2C);
        data(&mut h, 0xF800);
        assert_eq!(lcd(&h, id).gram_pixel(0, 0), Some(0xF800), "the swap follows the current MADCTL");
        // Mixed value: R=0b10101, G=0b110011, B=0b01010 swaps to R=0b01010, G unchanged, B=0b10101.
        command(&mut h, 0x36);
        data(&mut h, 0x08);
        command(&mut h, 0x2C);
        data(&mut h, 0b10101_110011_01010);
        assert_eq!(lcd(&h, id).gram_pixel(0, 0), Some(0b01010_110011_10101));
    }

    #[test]
    fn orientation_change_clears_the_gram_and_swaps_the_geometry() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        window(&mut h, 0, 239, 0, 319);
        command(&mut h, 0x2C);
        data(&mut h, 0x1234);
        assert_eq!(lcd(&h, id).non_black_pixels(), 1);
        let version = lcd(&h, id).frame_version();
        landscape(&mut h);
        let l = lcd(&h, id);
        assert_eq!((l.width(), l.height()), (320, 240));
        assert_eq!(l.non_black_pixels(), 0);
        assert_eq!(l.gram_pixel(0, 0), Some(0));
        assert_eq!(l.madctl(), 0x60);
        assert!(l.frame_version() > version, "the geometry change bumps the frame version");
        assert_eq!(l.describe().split("; ").next(), Some("320x240"));
        // Writing the same MADCTL again does not clear anything.
        window(&mut h, 0, 319, 0, 239);
        command(&mut h, 0x2C);
        data(&mut h, 0x4321);
        landscape(&mut h);
        assert_eq!(lcd(&h, id).non_black_pixels(), 1);
        // A landscape-orientation index uses the new width: pixel (319, 239) is the last GRAM entry.
        window(&mut h, 319, 319, 239, 239);
        command(&mut h, 0x2C);
        data(&mut h, 0x0001);
        assert_eq!(lcd(&h, id).gram_pixel(319, 239), Some(1));
        assert_eq!(lcd(&h, id).gram_pixel(320, 0), None);
        // Back to portrait (MADCTL 0): cleared again.
        command(&mut h, 0x36);
        data(&mut h, 0x00);
        assert_eq!((lcd(&h, id).width(), lcd(&h, id).height()), (240, 320));
        assert_eq!(lcd(&h, id).non_black_pixels(), 0);
    }

    #[test]
    fn parameters_beyond_the_buffer_and_other_commands_are_harmless() {
        let (mut h, id) = setup();
        command(&mut h, 0xB0); // RAMCTRL: vendor command with parameters, ignored
        data(&mut h, 0x00);
        data(&mut h, 0xE0);
        command(&mut h, 0xC6);
        for i in 0..40 {
            data(&mut h, i);
        }
        assert_eq!(lcd(&h, id).data_writes(), 42);
        assert_eq!(lcd(&h, id).pixel_writes(), 0);
        assert_eq!(lcd(&h, id).window(), (0, 239, 0, 319));
        // COLMOD: stored, warned about when not RGB565 (once per message).
        command(&mut h, 0x3A);
        data(&mut h, 0x06);
        assert_eq!(lcd(&h, id).color_mode(), 0x06);
        assert!(h.warnings().iter().any(|w| w.contains("LCD color mode 0x06 is not recovered RGB565")), "{:?}", h.warnings());
        command(&mut h, 0x3A);
        data(&mut h, 0x55);
        assert_eq!(lcd(&h, id).color_mode(), 0x55);
        assert_eq!(h.warnings().len(), 1, "0x55 & 7 == 5 is RGB565");
        // Data written before any command, and after commands that take no parameters, is a harmless argument.
        command(&mut h, 0x28);
        data(&mut h, 0x1234);
        assert_eq!(lcd(&h, id).window(), (0, 239, 0, 319));
    }

    #[test]
    fn unexpected_offsets_warn_once_and_change_nothing() {
        let (mut h, id) = setup();
        h.write16(BASE + 2, 0x11);
        h.write16(BASE + 2, 0x11);
        h.write32(BASE + 0x20000, 0x11);
        let l = lcd(&h, id);
        assert_eq!((l.command_writes(), l.data_writes()), (0, 0));
        assert!(l.sleeping());
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("Unexpected LCD write offset 0x2"), "{warnings:?}");
        assert!(warnings[1].contains("Unexpected LCD write offset 0x20000"), "{warnings:?}");
        // The word-sized store at the command offset behaves like the halfword write of its low 16 bits.
        h.write32(BASE, 0xABCD_0011);
        assert!(!lcd(&h, id).sleeping(), "0x11 sleep out");
    }

    #[test]
    fn visibility_follows_display_on_and_sleep() {
        let (mut h, id) = setup();
        window(&mut h, 0, 239, 0, 319);
        command(&mut h, 0x2C);
        data(&mut h, 0xFFFF);
        data(&mut h, 0x07E0);
        // Display off and asleep: the visible buffer stays black though the GRAM has content.
        assert_eq!(lcd(&h, id).non_black_pixels(), 2);
        let version = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[0], 0);
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), version, "black stays black: no new version");
        command(&mut h, 0x29);
        h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[0], 0, "display on but still sleeping");
        command(&mut h, 0x11);
        let on = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert!(on > version);
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[..2], [0xFFFF, 0x07E0]);
        // Sleeping again blanks it; the GRAM is kept and comes back on wake.
        command(&mut h, 0x10);
        let off = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert!(off > on);
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[..2], [0, 0]);
        command(&mut h, 0x11);
        let back = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert!(back > off);
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[..2], [0xFFFF, 0x07E0]);
        command(&mut h, 0x28);
        h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert_eq!(h.get::<NgcParallelLcd>(id).visible_rgb565()[..2], [0, 0], "display off blanks too");
    }

    #[test]
    fn frame_version_changes_only_when_visible_pixels_change() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        landscape(&mut h);
        window(&mut h, 0, 319, 0, 239);
        let v0 = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), v0, "nothing happened");
        command(&mut h, 0x2C);
        data(&mut h, 0x0000);
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), v0, "a black pixel over black changes nothing");
        data(&mut h, 0xF800);
        let v1 = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        assert!(v1 > v0);
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), v1, "idempotent");
        // Writing the value that is already there: no change.
        command(&mut h, 0x2C);
        data(&mut h, 0x0000);
        data(&mut h, 0xF800);
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), v1);
        // Overwrite and restore between two syncs: the GRAM was dirty but the visible pixels are the same.
        command(&mut h, 0x2C);
        data(&mut h, 0x1111);
        command(&mut h, 0x2C);
        data(&mut h, 0x0000);
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).sync_frame(), v1);
        // The 120 Hz repaint refreshes the buffer by itself.
        command(&mut h, 0x2C);
        data(&mut h, 0x2222);
        assert_eq!(h.get::<NgcParallelLcd>(id).frame_version(), v1, "not yet repainted");
        h.advance_to(PERIOD);
        assert!(h.get::<NgcParallelLcd>(id).frame_version() > v1, "the repaint tick rebuilt the buffer");
    }

    #[test]
    fn rgba_view_expands_components_like_the_ppm() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        landscape(&mut h);
        window(&mut h, 0, 319, 0, 239);
        command(&mut h, 0x2C);
        for pixel in [0xF800u32, 0x07E0, 0x001F, 0xFFFF, 0x0000, 0b01010_100001_00111] {
            data(&mut h, pixel);
        }
        let lcd = h.get_mut::<NgcParallelLcd>(id);
        lcd.sync_frame();
        let version = lcd.frame_version();
        let rgba = lcd.frame_rgba().to_vec();
        assert_eq!(rgba.len(), 320 * 240 * 4);
        assert_eq!(&rgba[..24], [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 82, 134, 57, 255]);
        assert!(rgba[24..].chunks(4).all(|px| px == [0, 0, 0, 255]));
        // RGBA, PPM and RGB565 views agree for every pixel.
        let ppm = lcd.export_ppm();
        assert_eq!(lcd.frame_version(), version, "export did not change the visible pixels");
        let header = b"P6\n320 240\n255\n";
        assert_eq!(&ppm[..header.len()], header);
        let body = &ppm[header.len()..];
        assert_eq!(body.len(), 320 * 240 * 3);
        for (i, px) in rgba.chunks(4).enumerate() {
            assert_eq!(&body[i * 3..i * 3 + 3], &px[..3], "pixel {i}");
        }
        assert_eq!(lcd.rgba_version, lcd.version, "the conversion is cached for this version");
        // Expansion tables.
        assert_eq!(EXPAND5[0], 0);
        assert_eq!(EXPAND5[31], 255);
        assert_eq!(EXPAND5[16], 132);
        assert_eq!(EXPAND6[63], 255);
        assert_eq!(EXPAND6[32], 130);
        for c in 0..32usize {
            assert_eq!(usize::from(EXPAND5[c]), (c << 3) | (c >> 2));
        }
        for c in 0..64usize {
            assert_eq!(usize::from(EXPAND6[c]), (c << 2) | (c >> 4));
        }
    }

    #[test]
    fn ppm_matches_the_renode_layout_in_portrait() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        window(&mut h, 239, 239, 319, 319);
        command(&mut h, 0x2C);
        data(&mut h, 0x8410);
        let ppm = h.get_mut::<NgcParallelLcd>(id).export_ppm();
        assert_eq!(&ppm[..15], b"P6\n240 320\n255\n");
        assert_eq!(ppm.len(), 15 + 240 * 320 * 3);
        assert_eq!(&ppm[ppm.len() - 3..], [132, 130, 132]);
        assert!(ppm[15..ppm.len() - 3].iter().all(|&b| b == 0));
    }

    #[test]
    fn te_pulses_at_the_renode_repaint_period() {
        let (mut h, id) = setup();
        let te = h.probe(id, TE);
        // Display not ready: the thread runs but TE stays low (and no edge is delivered).
        h.advance_to(5 * PERIOD);
        assert!(h.probe_changes(te).is_empty());
        bring_up(&mut h);
        let enabled_at = h.now();
        assert_eq!(enabled_at, 5 * PERIOD);
        h.advance_to(5 * PERIOD + 20 * PERIOD);
        let edges = h.probe_changes(te);
        assert_eq!(edges.len(), 20);
        for (i, &(time, level)) in edges.iter().enumerate() {
            assert_eq!(time, (6 + i as u64) * PERIOD, "edge {i}");
            assert_eq!(level, i % 2 == 0, "edge {i}");
        }
        assert_eq!(lcd(&h, id).te_pulses(), 10);
        // Rising edges are 2 * 8_333_334 ns apart: 59.9999976 Hz.
        assert_eq!(edges[2].0 - edges[0].0, 16_666_668);
        // TE off drives the line low at once (no wait for the next repaint) and stops the pulses.
        let t = h.now();
        h.advance_to(t + PERIOD); // rising edge pending state: edge 21 is high or low?
        let level_before = h.output(id, TE);
        command(&mut h, 0x34);
        assert!(!h.output(id, TE));
        let last = *h.probe_changes(te).last().unwrap();
        if level_before {
            assert_eq!(last, (h.now(), false), "falling edge at the command's time");
        }
        h.advance_to(h.now() + 10 * PERIOD);
        assert!(!h.output(id, TE));
        assert_eq!(h.probe_changes(te).last().unwrap(), &last);
        // TE on again resumes with a rising edge on the next tick.
        command(&mut h, 0x35);
        let before = h.probe_changes(te).len();
        h.advance_to(h.now() + 2 * PERIOD);
        assert!(h.probe_changes(te).len() > before);
    }

    #[test]
    fn te_requires_display_on_not_sleeping_and_simulate_te() {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, NgcParallelLcd::new("lcd", false));
        let te = h.probe(id, TE);
        bring_up(&mut h);
        h.advance_to(10 * PERIOD);
        assert!(h.probe_changes(te).is_empty(), "simulateTE: false keeps TE low");
        let (mut h, id) = setup();
        let te = h.probe(id, TE);
        command(&mut h, 0x35);
        command(&mut h, 0x29); // display on but asleep
        h.advance_to(4 * PERIOD);
        assert!(h.probe_changes(te).is_empty());
        command(&mut h, 0x11);
        h.advance_to(6 * PERIOD);
        assert_eq!(h.probe_changes(te), [(5 * PERIOD, true), (6 * PERIOD, false)]);
        command(&mut h, 0x10); // sleep in while TE is low: stays low
        h.advance_to(8 * PERIOD);
        assert_eq!(h.probe_changes(te).len(), 2);
        // Sleep in while TE is high drops it at the next tick.
        command(&mut h, 0x11);
        h.advance_to(9 * PERIOD);
        assert!(h.output(id, TE));
        command(&mut h, 0x10);
        h.advance_to(10 * PERIOD);
        assert!(!h.output(id, TE));
        assert_eq!(lcd(&h, id).te_pulses(), 2);
    }

    #[test]
    fn software_reset_zeroes_state_and_counters() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        landscape(&mut h);
        window(&mut h, 0, 10, 0, 10);
        command(&mut h, 0x2C);
        data(&mut h, 0xFFFF);
        command(&mut h, 0x04);
        h.read16(DATA);
        assert!(lcd(&h, id).command_writes() > 0);
        let version = h.get_mut::<NgcParallelLcd>(id).sync_frame();
        command(&mut h, 0x01);
        let l = lcd(&h, id);
        assert_eq!(
            l.describe(),
            "240x320; panelOn=False; sleeping=True; MADCTL=0x00; COLMOD=0x05; commands=0; data=0; pixels=0; nonBlackGRAM=0; reads=0; TE=0"
        );
        assert_eq!(l.window(), (0, 239, 0, 319));
        assert!(l.frame_version() > version, "the visible buffer was cleared");
        assert_eq!(h.get_mut::<NgcParallelLcd>(id).frame_rgba().len(), 240 * 320 * 4);
        // The repaint thread keeps its phase across a reset that changes the geometry.
        assert_eq!(h.core().clock_entry_count(), 1);
        assert_eq!(h.next_event_time(), Some(PERIOD));
        // TE enable does not survive the reset.
        let te = h.probe(id, TE);
        h.advance_to(4 * PERIOD);
        assert!(h.probe_changes(te).is_empty());
    }

    #[test]
    fn reset_input_is_active_low_and_connect_pushes_the_initial_level() {
        let (mut h, id) = setup();
        bring_up(&mut h);
        command(&mut h, 0x2C);
        data(&mut h, 0x1234);
        h.set_input(id, RESET_INPUT, true);
        assert_eq!(lcd(&h, id).pixel_writes(), 1, "a high level does nothing");
        h.set_input(id, RESET_INPUT, false);
        assert_eq!(lcd(&h, id).pixel_writes(), 0);
        assert_eq!(lcd(&h, id).non_black_pixels(), 0);
        assert!(lcd(&h, id).sleeping());
        assert!(!lcd(&h, id).panel_on());
        // Other input lines are ignored.
        bring_up(&mut h);
        h.set_input(id, 1, false);
        assert!(lcd(&h, id).panel_on());
        // `gpioB: 4 -> lcd@0` pushes the initial low level when the net is wired: that resets (an empty) LCD.
        let mut h2 = Harness::new();
        let lcd2 = h2.add_mapped(BASE, SIZE, NgcParallelLcd::new("lcd", true));
        let pb4 = h2.add(PinLike);
        h2.connect_input(pb4, 0, lcd2, 0);
        assert_eq!(h2.get::<NgcParallelLcd>(lcd2).describe().split("; ").nth(1), Some("panelOn=False"));
    }

    /// Stands in for `gpioB`: an output line 0 that is low.
    struct PinLike;
    impl Peripheral for PinLike {
        fn name(&self) -> &str {
            "pb"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        impl_peripheral_any!();
    }

    #[test]
    fn repaint_thread_period_is_ceil_of_120_hz() {
        let (mut h, _id) = setup();
        assert_eq!(h.next_event_time(), Some(8_333_334));
        h.advance_to(8_333_334);
        assert_eq!(h.next_event_time(), Some(16_666_668), "the overshoot is discarded: exactly one period later");
        h.advance_to(MS * 1000);
        assert_eq!(h.next_event_time(), Some(120 * PERIOD), "119 periods fit in one second, the 120th ends 80 ns after it");
    }

    /// Cost of the pixel path (printed, not asserted; run with `--nocapture`).
    #[test]
    #[ignore = "timing measurement"]
    fn pixel_write_cost() {
        use std::time::Instant;
        let (mut h, id) = setup();
        bring_up(&mut h);
        landscape(&mut h);
        window(&mut h, 0, 319, 0, 239);
        command(&mut h, 0x2C);
        let frames = 40u32;
        let start = Instant::now();
        for frame in 0..frames {
            for i in 0..76_800u32 {
                h.write16(DATA, (frame.wrapping_mul(7919) + i) & 0xFFFF);
            }
        }
        let framework = start.elapsed();
        let writes = f64::from(frames) * 76_800.0;
        println!("harness bus write (framework dispatch included): {:.1} ns/write", framework.as_secs_f64() * 1e9 / writes);
        let lcd = h.get_mut::<NgcParallelLcd>(id);
        let start = Instant::now();
        for frame in 0..frames {
            for i in 0..76_800u32 {
                lcd.write_pixel(((frame.wrapping_mul(7919) + i) & 0xFFFF) as u16);
            }
        }
        let direct = start.elapsed();
        println!("model write_pixel only: {:.2} ns/write", direct.as_secs_f64() * 1e9 / writes);
        assert!(lcd.pixel_writes() > 0);
    }
}
