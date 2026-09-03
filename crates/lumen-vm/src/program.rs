//! The program format: header, constant pool, palettes, three sections.
//!
//! ```text
//! magic        "LVM\0"
//! vm_version   u8    minimum VM version this program needs
//! profile      u8    0 pixel, 1 sim
//! program_id   u16
//! flags        u16   bit0 contains probes
//! const_count  u16   Q16 literals
//! palette_count u8
//! channel_count u8
//! once_len     u16   instructions
//! frame_len    u16
//! pixel_len    u16
//! budget       u32   compiler's estimated cost per invocation
//! graph_hash   u64   source graph hash
//! channels     channel_count x u16
//! constants    const_count x i32
//! palettes     palette_count x (PALETTE_STOPS x 3 x i32)
//! once         once_len x 4 bytes
//! frame        frame_len x 4 bytes
//! pixel        pixel_len x 4 bytes
//! ```
//!
//! A [`Program`] borrows the bytes it was parsed from and copies nothing. On a
//! device the bytes live in flash and stay there.
//!
//! Parsing validates **structure**, not semantics: that every section is
//! present, every instruction decodes, and a `pixel` program uses no sim-only
//! instruction. Register indices and array bounds are checked at execution,
//! where the values are known.

use crate::isa::{Instruction, OpCode, INSTRUCTION_LEN};
use crate::q16::Q16;
use crate::{Fault, Profile};

/// First four bytes of every program.
pub const MAGIC: [u8; 4] = *b"LVM\0";

/// VM version this build implements. A program needing more is refused.
pub const VM_VERSION: u8 = 1;

/// Colour stops per palette. Evenly spaced; sampling interpolates between them.
pub const PALETTE_STOPS: usize = 16;

const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 2 + 2 + 1 + 1 + 2 + 2 + 2 + 4 + 8;

/// Why a program could not be loaded.
///
/// Separate from [`Fault`] because these are decided once at load, not per
/// pixel: whatever is wrong here is wrong before the first frame, and the
/// device should say so rather than fault three hundred times a second.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramError {
    /// Not a program.
    BadMagic,
    /// Ran off the end.
    Truncated,
    /// Needs a newer VM than this device implements.
    ///
    /// The only failure a firmware upgrade fixes, and the reason the instruction
    /// set is append-only: this never happens to a program that already runs.
    VmTooOld { needs: u8, have: u8 },
    /// Profile byte was not one of the two.
    BadProfile(u8),
    /// An instruction this VM does not implement.
    UnsupportedInstruction(u8),
    /// A sim-only instruction in a `pixel` program.
    ///
    /// Refused at load: reaching the per-pixel path with bounded loops in it
    /// would blow the budget three hundred times a frame.
    SimInstructionInPixelProfile(u8),
    /// A `REPEAT` with no matching `ENDREP`, or the reverse.
    UnbalancedRepeat,
    /// A `CALL` or `MASKTEST` pointing outside its section.
    BadJumpTarget,
}

/// Which of the three sections to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    /// On activation. Sees constants and device config.
    Once,
    /// Once per frame. Sees show time, channel uniforms, automation.
    Frame,
    /// Once per LED. Sees everything above plus this LED's position and index.
    Pixel,
}

/// A parsed program, borrowing the bytes it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Program<'a> {
    pub vm_version: u8,
    pub profile: Profile,
    pub program_id: u16,
    /// True when the program was built with `PROBE` instrumentation.
    ///
    /// Explicit because probes cost budget: a normal build contains none, so
    /// debugging never makes the shipped program slower.
    pub has_probes: bool,
    /// The compiler's estimated cost per invocation, in the same units as
    /// [`OpCode::cost`].
    pub budget: u32,
    /// Identifies the source graph, so an editor can recognise a program already
    /// running on a device and skip the upload.
    pub graph_hash: u64,
    channels: &'a [u8],
    constants: &'a [u8],
    palettes: &'a [u8],
    once: &'a [u8],
    frame: &'a [u8],
    pixel: &'a [u8],
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl<'a> Program<'a> {
    /// Parse and structurally validate a program.
    pub fn parse(bytes: &'a [u8]) -> Result<Program<'a>, ProgramError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProgramError::Truncated);
        }
        if bytes[0..4] != MAGIC {
            return Err(ProgramError::BadMagic);
        }
        let vm_version = bytes[4];
        if vm_version > VM_VERSION {
            return Err(ProgramError::VmTooOld {
                needs: vm_version,
                have: VM_VERSION,
            });
        }
        let profile = Profile::from_u8(bytes[5]).ok_or(ProgramError::BadProfile(bytes[5]))?;
        let program_id = le_u16(bytes, 6);
        let flags = le_u16(bytes, 8);
        let const_count = le_u16(bytes, 10) as usize;
        let palette_count = bytes[12] as usize;
        let channel_count = bytes[13] as usize;
        let once_len = le_u16(bytes, 14) as usize;
        let frame_len = le_u16(bytes, 16) as usize;
        let pixel_len = le_u16(bytes, 18) as usize;
        let budget = le_u32(bytes, 20);
        let graph_hash = u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
        ]);

        let mut at = HEADER_LEN;
        let mut take = |n: usize| -> Result<&'a [u8], ProgramError> {
            let end = at.checked_add(n).ok_or(ProgramError::Truncated)?;
            if end > bytes.len() {
                return Err(ProgramError::Truncated);
            }
            let out = &bytes[at..end];
            at = end;
            Ok(out)
        };

        let channels = take(channel_count * 2)?;
        let constants = take(const_count * 4)?;
        let palettes = take(palette_count * PALETTE_STOPS * 3 * 4)?;
        let once = take(once_len * INSTRUCTION_LEN)?;
        let frame = take(frame_len * INSTRUCTION_LEN)?;
        let pixel = take(pixel_len * INSTRUCTION_LEN)?;

        let program = Program {
            vm_version,
            profile,
            program_id,
            has_probes: flags & 1 != 0,
            budget,
            graph_hash,
            channels,
            constants,
            palettes,
            once,
            frame,
            pixel,
        };

        for section in [Section::Once, Section::Frame, Section::Pixel] {
            program.validate_section(section)?;
        }
        Ok(program)
    }

    /// Every instruction decodes, every jump lands inside the section, and
    /// `REPEAT`/`ENDREP` balance.
    ///
    /// Done once at load so the interpreter never has to re-check any of it.
    fn validate_section(&self, section: Section) -> Result<(), ProgramError> {
        let code = self.section(section);
        let count = code.len() / INSTRUCTION_LEN;
        let mut depth: i32 = 0;
        for idx in 0..count {
            let at = idx * INSTRUCTION_LEN;
            let raw = [code[at], code[at + 1], code[at + 2], code[at + 3]];
            let ins = Instruction::decode(raw).map_err(|e| match e {
                Fault::UnsupportedInstruction(b) => ProgramError::UnsupportedInstruction(b),
                _ => ProgramError::Truncated,
            })?;
            if self.profile == Profile::Pixel && ins.op.is_sim_only() {
                return Err(ProgramError::SimInstructionInPixelProfile(ins.op.to_u8()));
            }
            match ins.op {
                OpCode::Repeat => depth += 1,
                OpCode::EndRep => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(ProgramError::UnbalancedRepeat);
                    }
                }
                OpCode::Call if ins.bc() as usize >= count => {
                    return Err(ProgramError::BadJumpTarget);
                }
                // A skip that runs past the end would silently truncate the
                // section instead of doing what the author wrote.
                OpCode::MaskTest if idx + 1 + ins.bc() as usize > count => {
                    return Err(ProgramError::BadJumpTarget);
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(ProgramError::UnbalancedRepeat);
        }
        Ok(())
    }

    /// Raw bytes of one section.
    pub fn section(&self, section: Section) -> &'a [u8] {
        match section {
            Section::Once => self.once,
            Section::Frame => self.frame,
            Section::Pixel => self.pixel,
        }
    }

    /// Instruction count in a section.
    pub fn section_len(&self, section: Section) -> usize {
        self.section(section).len() / INSTRUCTION_LEN
    }

    /// One instruction, already validated at load.
    pub fn instruction(&self, section: Section, idx: usize) -> Option<Instruction> {
        let code = self.section(section);
        let at = idx * INSTRUCTION_LEN;
        if at + INSTRUCTION_LEN > code.len() {
            return None;
        }
        Instruction::decode([code[at], code[at + 1], code[at + 2], code[at + 3]]).ok()
    }

    /// A constant-pool entry.
    pub fn constant(&self, idx: u16) -> Option<Q16> {
        let at = idx as usize * 4;
        if at + 4 > self.constants.len() {
            return None;
        }
        Some(Q16(le_u32(self.constants, at) as i32))
    }

    pub fn constant_count(&self) -> usize {
        self.constants.len() / 4
    }

    /// Channel ids this program reads, in slot order.
    ///
    /// `CHREAD` names a *slot*, not a channel id, so the same program can be
    /// pointed at a different channel without recompiling.
    pub fn channel_id(&self, slot: u8) -> Option<u16> {
        let at = slot as usize * 2;
        if at + 2 > self.channels.len() {
            return None;
        }
        Some(le_u16(self.channels, at))
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len() / 2
    }

    pub fn palette_count(&self) -> usize {
        self.palettes.len() / (PALETTE_STOPS * 3 * 4)
    }

    /// Sample palette `idx` at `pos`, wrapping so a position of 1.0 meets 0.0.
    ///
    /// Palettes are evenly spaced stops with linear interpolation between them —
    /// cheap enough for the per-pixel path, and the compiler bakes any authored
    /// gradient down to this at publish time.
    pub fn palette_sample(&self, idx: u8, pos: Q16) -> Option<(Q16, Q16, Q16)> {
        let stride = PALETTE_STOPS * 3 * 4;
        let base = idx as usize * stride;
        if base + stride > self.palettes.len() {
            return None;
        }
        let stop = |i: usize| -> (Q16, Q16, Q16) {
            let at = base + (i % PALETTE_STOPS) * 12;
            (
                Q16(le_u32(self.palettes, at) as i32),
                Q16(le_u32(self.palettes, at + 4) as i32),
                Q16(le_u32(self.palettes, at + 8) as i32),
            )
        };
        let scaled = pos.fract().mul(Q16::from_int(PALETTE_STOPS as i16));
        let i = scaled.to_int() as usize;
        let t = scaled.fract();
        let (r0, g0, b0) = stop(i);
        let (r1, g1, b1) = stop(i + 1);
        Some((r0.lerp(r1, t), g0.lerp(g1, t), b0.lerp(b1, t)))
    }
}

/// Builds programs, for tests and for the compiler's emitter.
///
/// Lives here rather than in a test module because `lumen-lang` needs exactly
/// this and duplicating the layout in two crates is how the two drift apart.
#[cfg(feature = "builder")]
pub mod builder {
    use super::*;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Assembles a program image.
    #[derive(Clone, Default)]
    pub struct ProgramBuilder {
        pub program_id: u16,
        pub profile_sim: bool,
        pub has_probes: bool,
        pub budget: u32,
        pub graph_hash: u64,
        channels: Vec<u16>,
        constants: Vec<i32>,
        palettes: Vec<i32>,
        once: Vec<Instruction>,
        frame: Vec<Instruction>,
        pixel: Vec<Instruction>,
    }

    impl ProgramBuilder {
        pub fn new() -> Self {
            Self::default()
        }

        /// Add a constant, returning its pool index. Identical values are
        /// shared, which keeps a program with one number used forty times from
        /// carrying forty copies of it.
        pub fn constant(&mut self, v: Q16) -> u16 {
            if let Some(i) = self.constants.iter().position(|&c| c == v.0) {
                return i as u16;
            }
            self.constants.push(v.0);
            (self.constants.len() - 1) as u16
        }

        /// Bind a channel id to the next slot, returning the slot.
        pub fn channel(&mut self, id: u16) -> u8 {
            self.channels.push(id);
            (self.channels.len() - 1) as u8
        }

        /// Add a palette of exactly [`PALETTE_STOPS`] RGB stops.
        pub fn palette(&mut self, stops: &[(Q16, Q16, Q16); PALETTE_STOPS]) -> u8 {
            for (r, g, b) in stops {
                self.palettes.push(r.0);
                self.palettes.push(g.0);
                self.palettes.push(b.0);
            }
            (self.palettes.len() / (PALETTE_STOPS * 3) - 1) as u8
        }

        pub fn push(&mut self, section: Section, ins: Instruction) -> &mut Self {
            match section {
                Section::Once => self.once.push(ins),
                Section::Frame => self.frame.push(ins),
                Section::Pixel => self.pixel.push(ins),
            }
            self
        }

        /// Serialise. The budget is recomputed from the instructions actually
        /// emitted unless one was set explicitly, so it cannot drift from the
        /// code it describes.
        pub fn build(&self) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&MAGIC);
            out.push(VM_VERSION);
            out.push(if self.profile_sim { 1 } else { 0 });
            out.extend_from_slice(&self.program_id.to_le_bytes());
            out.extend_from_slice(&(if self.has_probes { 1u16 } else { 0 }).to_le_bytes());
            out.extend_from_slice(&(self.constants.len() as u16).to_le_bytes());
            out.push((self.palettes.len() / (PALETTE_STOPS * 3)) as u8);
            out.push(self.channels.len() as u8);
            out.extend_from_slice(&(self.once.len() as u16).to_le_bytes());
            out.extend_from_slice(&(self.frame.len() as u16).to_le_bytes());
            out.extend_from_slice(&(self.pixel.len() as u16).to_le_bytes());
            let budget = if self.budget != 0 {
                self.budget
            } else {
                self.pixel.iter().map(|i| i.op.cost()).sum()
            };
            out.extend_from_slice(&budget.to_le_bytes());
            out.extend_from_slice(&self.graph_hash.to_le_bytes());
            for c in &self.channels {
                out.extend_from_slice(&c.to_le_bytes());
            }
            for k in &self.constants {
                out.extend_from_slice(&k.to_le_bytes());
            }
            for p in &self.palettes {
                out.extend_from_slice(&p.to_le_bytes());
            }
            for section in [&self.once, &self.frame, &self.pixel] {
                for ins in section {
                    out.extend_from_slice(&ins.encode());
                }
            }
            out
        }
    }
}
