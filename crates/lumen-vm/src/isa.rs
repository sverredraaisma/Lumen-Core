//! The instruction set.
//!
//! **Fixed four-byte instructions**: `opcode, a, b, c`. Not the densest possible
//! encoding, and deliberately so — dispatch dominates an interpreted per-pixel
//! kernel, and a fixed stride means decoding is three array reads with no
//! branching. It also makes `MASKTEST`'s skip distance a simple instruction
//! count, which is what lets a masked-off pixel cost a handful of instructions
//! instead of a whole layer stack.
//!
//! # Append-only
//!
//! **The instruction set is append-only within a VM major version.** A program
//! declares the minimum VM version it needs; a device refuses anything requiring
//! more than it implements and says so. The consequence worth protecting: a
//! firmware upgrade never invalidates a running program. New instructions only
//! ever gate *new* effects on *old* devices, which is a failure an app can
//! explain.
//!
//! So: never renumber an opcode, never change what one does, never remove one.
//! Capability grows in the versioned standard library, not here.

use crate::Fault;

/// Number of registers. Five bits would do; the encoding has a whole byte, and
/// the spare range is used to catch bad programs rather than silently masked.
pub const REG_COUNT: usize = 32;

/// A register index, already checked against [`REG_COUNT`].
pub type Reg = u8;

/// Bytes per encoded instruction.
pub const INSTRUCTION_LEN: usize = 4;

/// An opcode.
///
/// Grouped by high nibble so dispatch stays a jump table and each family has
/// obvious room to grow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OpCode {
    // --- 0x0_ core ---------------------------------------------------------
    Nop = 0x00,
    /// `a = b`
    Mov = 0x01,
    /// `a = constants[bc]`
    LoadK = 0x02,
    /// `a = b + c`
    Add = 0x03,
    Sub = 0x04,
    Mul = 0x05,
    Div = 0x06,
    /// `a = a * b + c`, with the product kept at full width before the add.
    Madd = 0x07,
    /// `a = -b`
    Neg = 0x08,
    Abs = 0x09,
    Min = 0x0A,
    Max = 0x0B,
    /// `a = clamp(a, b, c)`
    Clamp = 0x0C,
    Floor = 0x0D,
    Fract = 0x0E,

    // --- 0x1_ transcendental (table-driven) --------------------------------
    /// `a = sin(b)`, radians.
    Sin = 0x10,
    Cos = 0x11,
    /// `a = sin(b)` where `b` is in turns. Cheaper and exact at the cardinal
    /// points, which is what most effects actually want.
    SinTurns = 0x12,
    CosTurns = 0x13,
    /// `a = atan2(b, c)`
    Atan2 = 0x14,
    Sqrt = 0x15,
    /// `a = b ^ c`
    Pow = 0x16,
    Exp = 0x17,
    /// Natural logarithm.
    Log = 0x18,
    Log2 = 0x19,

    // --- 0x2_ noise --------------------------------------------------------
    /// `a = noise(b)`
    Noise1 = 0x20,
    /// `a = noise(b, c)`
    Noise2 = 0x21,
    /// `a = noise(b, b+1, b+2)` — three consecutive registers starting at `b`.
    Noise3 = 0x22,

    // --- 0x3_ compare and select -------------------------------------------
    /// `a = (b < c) ? 1 : 0`
    Lt = 0x30,
    Le = 0x31,
    Gt = 0x32,
    Ge = 0x33,
    Eq = 0x34,
    /// `a = (a != 0) ? b : c`. Branchless, which is the point.
    Select = 0x35,
    /// `a = step(b, edge = c)`
    Step = 0x36,
    /// `a = smoothstep(a, e0 = b, e1 = c)`
    SmoothStep = 0x37,
    /// `a = lerp(a, b, t = c)`
    Lerp = 0x38,

    // --- 0x4_ space --------------------------------------------------------
    /// `a = length(b, c)`
    Len2 = 0x40,
    /// `a = length(b, b+1, b+2)`
    Len3 = 0x41,
    /// `a = dot((b, b+1, b+2), (c, c+1, c+2))`
    Dot3 = 0x42,

    // --- 0x5_ colour -------------------------------------------------------
    /// `(a, a+1, a+2) = hsv_to_rgb(b, b+1, b+2)`
    Hsv2Rgb = 0x50,
    /// `(a, a+1, a+2) = rgb_to_hsv(b, b+1, b+2)`
    Rgb2Hsv = 0x51,
    /// `(a, a+1, a+2) = palette[c] sampled at position b`
    Palette = 0x52,
    /// `(a, a+1, a+2) = kelvin_to_rgb(b)`
    Temp2Rgb = 0x53,

    // --- 0x6_ uniforms and history -----------------------------------------
    /// `a = channel[b][c]` — a `CHAN` uniform.
    ChRead = 0x60,
    /// `a = prev` — this pixel's value last frame.
    PrevRead = 0x61,
    /// `prev = a`
    PrevWrite = 0x62,

    // --- 0x7_ control flow -------------------------------------------------
    /// If register `a` is zero, skip forward `bc` instructions.
    ///
    /// The early-out that makes layered effects affordable. Forward only, and
    /// the distance is known at compile time.
    MaskTest = 0x70,
    /// Begin a block repeated `bc` times. Trip count is a compile-time constant.
    Repeat = 0x71,
    EndRep = 0x72,
    /// Call the subroutine at instruction index `bc`.
    Call = 0x73,
    Ret = 0x74,
    /// End this section.
    Halt = 0x75,

    // --- 0x8_ arrays -------------------------------------------------------
    /// `a = array[b][c]`, bounds-checked.
    ///
    /// Legal in **both** profiles. A sim accessor is a bounded accumulation
    /// running per pixel over the broadcast simulation state, so the pixel
    /// kernel has to be able to read it. Writing it, and asking how long it is,
    /// remain the sim master's alone.
    ALoad = 0x80,
    /// `array[a][b] = c`, bounds-checked. Sim profile only.
    ///
    /// Three hundred LEDs on forty devices all writing one array would need an
    /// ordering rule the sans-IO design deliberately does not have.
    AStore = 0x81,
    /// `a = len(array[b])`. Sim profile only.
    ///
    /// A trip count discovered at run time could not be costed before the
    /// program was published, and an exact budget is the point.
    ALen = 0x82,

    // --- 0x9_ debug (probe builds only) ------------------------------------
    /// Record register `a` to probe `bc`.
    ///
    /// Costs budget like any other instruction, so probe builds are explicit and
    /// bounded. A normal build contains none of these: debugging must never make
    /// the shipped program slower.
    Probe = 0x90,

    // --- 0xA_ output -------------------------------------------------------
    /// Emit `(r, g, b)` from registers `a`, `b`, `c`.
    EmitRgb = 0xA0,
    /// Emit `(r, g, b, w)` from four consecutive registers starting at `a`.
    EmitRgbw = 0xA1,
    /// Emit correlated colour temperature `a` at intensity `b`.
    EmitCct = 0xA2,
}

impl OpCode {
    /// Map a byte to an opcode, or `None` if this VM does not implement it.
    pub const fn from_u8(v: u8) -> Option<OpCode> {
        use OpCode::*;
        Some(match v {
            0x00 => Nop,
            0x01 => Mov,
            0x02 => LoadK,
            0x03 => Add,
            0x04 => Sub,
            0x05 => Mul,
            0x06 => Div,
            0x07 => Madd,
            0x08 => Neg,
            0x09 => Abs,
            0x0A => Min,
            0x0B => Max,
            0x0C => Clamp,
            0x0D => Floor,
            0x0E => Fract,
            0x10 => Sin,
            0x11 => Cos,
            0x12 => SinTurns,
            0x13 => CosTurns,
            0x14 => Atan2,
            0x15 => Sqrt,
            0x16 => Pow,
            0x17 => Exp,
            0x18 => Log,
            0x19 => Log2,
            0x20 => Noise1,
            0x21 => Noise2,
            0x22 => Noise3,
            0x30 => Lt,
            0x31 => Le,
            0x32 => Gt,
            0x33 => Ge,
            0x34 => Eq,
            0x35 => Select,
            0x36 => Step,
            0x37 => SmoothStep,
            0x38 => Lerp,
            0x40 => Len2,
            0x41 => Len3,
            0x42 => Dot3,
            0x50 => Hsv2Rgb,
            0x51 => Rgb2Hsv,
            0x52 => Palette,
            0x53 => Temp2Rgb,
            0x60 => ChRead,
            0x61 => PrevRead,
            0x62 => PrevWrite,
            0x70 => MaskTest,
            0x71 => Repeat,
            0x72 => EndRep,
            0x73 => Call,
            0x74 => Ret,
            0x75 => Halt,
            0x80 => ALoad,
            0x81 => AStore,
            0x82 => ALen,
            0x90 => Probe,
            0xA0 => EmitRgb,
            0xA1 => EmitRgbw,
            0xA2 => EmitCct,
            _ => return None,
        })
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// True for instructions only the `sim` profile may use.
    ///
    /// Checked at load rather than at execution: a `sim` program reaching the
    /// per-pixel path would blow the budget three hundred times a frame, and
    /// finding that out per-pixel is far too late.
    /// Whether this instruction may appear only in a `sim` program.
    ///
    /// `ALoad` is deliberately absent: a pixel kernel reads the broadcast sim
    /// state to evaluate an accessor. It still cannot write it (`AStore`) or ask
    /// its length (`ALen`), which is what keeps shared state single-writer and
    /// keeps a per-pixel accumulation's cost known before it ships.
    pub const fn is_sim_only(self) -> bool {
        matches!(self, OpCode::AStore | OpCode::ALen)
    }

    /// Estimated cost in budget units.
    ///
    /// The compiler multiplies these by LED count and frame rate to answer "will
    /// this run at 60 fps on that device" before publishing. They are relative
    /// weights, not cycle counts: table lookups and colour conversions really do
    /// cost several times a `MOV`, and pretending otherwise would make the
    /// budget report useless exactly where it matters.
    pub const fn cost(self) -> u32 {
        use OpCode::*;
        match self {
            Nop => 1,
            Mov | LoadK | Add | Sub | Neg | Abs | Min | Max | Clamp | Floor | Fract => 1,
            Mul | Madd | Lt | Le | Gt | Ge | Eq | Select | Step | Lerp => 2,
            Div => 6,
            SinTurns | CosTurns => 5,
            Sin | Cos => 7,
            Sqrt | Len2 => 8,
            Len3 | Dot3 | SmoothStep => 9,
            Atan2 | Log | Log2 | Exp => 12,
            Pow => 24,
            Noise1 => 10,
            Noise2 => 18,
            Noise3 => 28,
            Hsv2Rgb | Rgb2Hsv | Temp2Rgb | Palette => 12,
            ChRead | PrevRead | PrevWrite => 2,
            MaskTest | Repeat | EndRep | Call | Ret | Halt => 1,
            ALoad | AStore | ALen => 2,
            Probe => 3,
            EmitRgb | EmitRgbw | EmitCct => 2,
        }
    }
}

/// One decoded instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Instruction {
    pub op: OpCode,
    pub a: u8,
    pub b: u8,
    pub c: u8,
}

impl Instruction {
    pub const fn new(op: OpCode, a: u8, b: u8, c: u8) -> Instruction {
        Instruction { op, a, b, c }
    }

    /// `b` and `c` read together as a little-endian 16-bit immediate.
    ///
    /// Used by `LOADK`, `MASKTEST`, `REPEAT`, `CALL` and `PROBE`.
    pub const fn bc(self) -> u16 {
        u16::from_le_bytes([self.b, self.c])
    }

    /// Build one carrying a 16-bit immediate.
    pub const fn with_imm(op: OpCode, a: u8, imm: u16) -> Instruction {
        let [b, c] = imm.to_le_bytes();
        Instruction { op, a, b, c }
    }

    /// Decode from exactly [`INSTRUCTION_LEN`] bytes.
    pub const fn decode(bytes: [u8; INSTRUCTION_LEN]) -> Result<Instruction, Fault> {
        match OpCode::from_u8(bytes[0]) {
            Some(op) => Ok(Instruction {
                op,
                a: bytes[1],
                b: bytes[2],
                c: bytes[3],
            }),
            None => Err(Fault::UnsupportedInstruction(bytes[0])),
        }
    }

    pub const fn encode(self) -> [u8; INSTRUCTION_LEN] {
        [self.op.to_u8(), self.a, self.b, self.c]
    }
}

/// Check a register index, turning a bad program into a fault rather than a
/// silent wraparound into someone else's register.
pub const fn check_reg(r: u8) -> Result<usize, Fault> {
    if (r as usize) < REG_COUNT {
        Ok(r as usize)
    } else {
        Err(Fault::BadRegister(r))
    }
}

/// Check a run of `n` consecutive registers starting at `r`.
///
/// `NOISE3`, `LEN3`, the colour conversions and `EMIT_RGBW` all read or write
/// consecutive registers, so the last one has to be in range too — otherwise a
/// program could write past the register file by starting near the end of it.
pub const fn check_reg_run(r: u8, n: u8) -> Result<usize, Fault> {
    let last = r as usize + n as usize - 1;
    if last < REG_COUNT {
        Ok(r as usize)
    } else {
        Err(Fault::BadRegister(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every opcode this VM implements. Kept explicit so adding one without
    /// deciding its cost and its round trip is a test failure.
    const ALL: &[OpCode] = &[
        OpCode::Nop,
        OpCode::Mov,
        OpCode::LoadK,
        OpCode::Add,
        OpCode::Sub,
        OpCode::Mul,
        OpCode::Div,
        OpCode::Madd,
        OpCode::Neg,
        OpCode::Abs,
        OpCode::Min,
        OpCode::Max,
        OpCode::Clamp,
        OpCode::Floor,
        OpCode::Fract,
        OpCode::Sin,
        OpCode::Cos,
        OpCode::SinTurns,
        OpCode::CosTurns,
        OpCode::Atan2,
        OpCode::Sqrt,
        OpCode::Pow,
        OpCode::Exp,
        OpCode::Log,
        OpCode::Log2,
        OpCode::Noise1,
        OpCode::Noise2,
        OpCode::Noise3,
        OpCode::Lt,
        OpCode::Le,
        OpCode::Gt,
        OpCode::Ge,
        OpCode::Eq,
        OpCode::Select,
        OpCode::Step,
        OpCode::SmoothStep,
        OpCode::Lerp,
        OpCode::Len2,
        OpCode::Len3,
        OpCode::Dot3,
        OpCode::Hsv2Rgb,
        OpCode::Rgb2Hsv,
        OpCode::Palette,
        OpCode::Temp2Rgb,
        OpCode::ChRead,
        OpCode::PrevRead,
        OpCode::PrevWrite,
        OpCode::MaskTest,
        OpCode::Repeat,
        OpCode::EndRep,
        OpCode::Call,
        OpCode::Ret,
        OpCode::Halt,
        OpCode::ALoad,
        OpCode::AStore,
        OpCode::ALen,
        OpCode::Probe,
        OpCode::EmitRgb,
        OpCode::EmitRgbw,
        OpCode::EmitCct,
    ];

    #[test]
    fn every_opcode_maps_both_ways() {
        for &op in ALL {
            assert_eq!(OpCode::from_u8(op.to_u8()), Some(op), "{op:?}");
        }
    }

    #[test]
    fn opcode_numbers_are_unique() {
        // Two opcodes sharing a byte would make one of them unreachable, and the
        // set is append-only so a collision is permanent.
        for (i, &a) in ALL.iter().enumerate() {
            for &b in &ALL[i + 1..] {
                assert_ne!(a.to_u8(), b.to_u8(), "{a:?} and {b:?} share a byte");
            }
        }
    }

    #[test]
    fn unassigned_bytes_are_unsupported_rather_than_misread() {
        // "Upgrade the firmware" is a comprehensible thing to tell a user;
        // executing a neighbouring instruction instead is not.
        let known: [bool; 256] = {
            let mut k = [false; 256];
            for &op in ALL {
                k[op.to_u8() as usize] = true;
            }
            k
        };
        for byte in 0..=255u8 {
            if !known[byte as usize] {
                assert_eq!(OpCode::from_u8(byte), None, "byte {byte:#04x}");
                assert_eq!(
                    Instruction::decode([byte, 0, 0, 0]),
                    Err(Fault::UnsupportedInstruction(byte))
                );
            }
        }
    }

    #[test]
    fn every_opcode_costs_something() {
        // A zero-cost instruction would let a program loop for free, which is
        // exactly what the budget exists to prevent.
        for &op in ALL {
            assert!(op.cost() >= 1, "{op:?} costs nothing");
        }
    }

    #[test]
    fn expensive_instructions_cost_more_than_cheap_ones() {
        // The budget report is only useful if the weights are ordered sensibly.
        assert!(OpCode::Pow.cost() > OpCode::Mul.cost());
        assert!(OpCode::Noise3.cost() > OpCode::Noise1.cost());
        assert!(OpCode::Div.cost() > OpCode::Add.cost());
        assert!(OpCode::Sin.cost() > OpCode::SinTurns.cost());
    }

    #[test]
    fn only_writing_and_measuring_an_array_is_sim_only() {
        // `ALoad` is the exception and the reason this test is worth having: a
        // pixel kernel reads the broadcast sim state to evaluate an accessor,
        // and quietly adding it back to this set would make every effect that
        // uses one refuse to load with no diagnostic pointing here.
        for &op in ALL {
            let expect = matches!(op, OpCode::AStore | OpCode::ALen);
            assert_eq!(op.is_sim_only(), expect, "{op:?}");
        }
        assert!(
            !OpCode::ALoad.is_sim_only(),
            "a pixel kernel may read an array"
        );
    }

    #[test]
    fn instructions_round_trip() {
        for &op in ALL {
            let i = Instruction::new(op, 1, 2, 3);
            assert_eq!(Instruction::decode(i.encode()), Ok(i));
        }
    }

    #[test]
    fn immediates_pack_into_b_and_c() {
        let i = Instruction::with_imm(OpCode::LoadK, 5, 0xBEEF);
        assert_eq!(i.a, 5);
        assert_eq!(i.bc(), 0xBEEF);
        assert_eq!(Instruction::decode(i.encode()).unwrap().bc(), 0xBEEF);
        assert_eq!(Instruction::with_imm(OpCode::Repeat, 0, 0).bc(), 0);
        assert_eq!(
            Instruction::with_imm(OpCode::Repeat, 0, u16::MAX).bc(),
            u16::MAX
        );
    }

    #[test]
    fn register_checks_reject_out_of_range_indices() {
        assert_eq!(check_reg(0), Ok(0));
        assert_eq!(check_reg(31), Ok(31));
        assert_eq!(check_reg(32), Err(Fault::BadRegister(32)));
        assert_eq!(check_reg(255), Err(Fault::BadRegister(255)));
    }

    #[test]
    fn a_register_run_must_fit_entirely() {
        // The case that matters: a run starting inside the file but ending past
        // it would let a program write past the registers.
        assert_eq!(check_reg_run(29, 3), Ok(29));
        assert_eq!(check_reg_run(30, 3), Err(Fault::BadRegister(30)));
        assert_eq!(check_reg_run(31, 1), Ok(31));
        assert_eq!(check_reg_run(28, 4), Ok(28));
        assert_eq!(check_reg_run(29, 4), Err(Fault::BadRegister(29)));
    }
}
