//! The x87 FPU. Registers are doubles in `Cpu::st` (the CRT sets 53-bit precision, so every
//! result is rounded to a double anyway); TOP is a Cranelift variable like the integer
//! registers, so ST(i) is the slot `(top + i) & 7`, found at run time. Things that are rare
//! or need libm (transcendental functions, 80-bit memory operands) go to `rt_x87`.
//!
//! Along a straight run of instructions the registers live in Cranelift values (`Cache`):
//! the renderer's projection loops are long chains of fld, fmul, fxch, faddp, which each
//! loaded and stored their slots. The cache knows ST(i) by its place relative to TOP at the
//! run's start, written back (`x87_settle`) before an instruction reached from elsewhere,
//! anything that branches, calls or runs a helper, and every instruction but plain integer
//! ones. The exact integer of a fild qword (`Cpu::st_int`) stays in memory and does not move
//! with fxch or fld st(i): a fistp qword finds it only in the slot fild loaded it into (it
//! checks the double's bits, so a moved value gives the rounded one: integers beyond 2^53).

use super::{Lifter, mf, ty};
use crate::rt::{Cpu, FpuOp, X87};
use cranelift_codegen::ir::{Endianness, InstBuilder, Value, condcodes::FloatCC, condcodes::IntCC, types::*};
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use std::mem::offset_of;

const C0: i64 = 0x100;
const C2: i64 = 0x400;
const C3: i64 = 0x4000;

pub fn is_x87(m: Mnemonic) -> bool {
    use Mnemonic::*;
    matches!(
        m,
        Fld | Fst | Fstp | Fild | Fist | Fistp | Fisttp | Fxch | Fadd | Faddp | Fiadd | Fsub | Fsubp | Fisub | Fsubr
            | Fsubrp | Fisubr | Fmul | Fmulp | Fimul | Fdiv | Fdivp | Fidiv | Fdivr | Fdivrp | Fidivr | Fchs | Fabs
            | Fsqrt | Frndint | Fld1 | Fldz | Fldpi | Fldl2e | Fldl2t | Fldlg2 | Fldln2 | Fcom | Fcomp | Fcompp
            | Fucom | Fucomp | Fucompp | Ficom | Ficomp | Ftst | Fcomi | Fcomip | Fucomi | Fucomip | Fnstsw | Fnstcw
            | Fldcw | Fnclex | Fninit | Ffree | Fincstp | Fdecstp | Fnop | Fxam | Fsin | Fcos | Fsincos | Fptan
            | Fpatan | F2xm1 | Fyl2x | Fyl2xp1 | Fscale | Fprem | Fprem1 | Fxtract
    )
}

/// Why an x87 instruction cannot be lifted, if it cannot (operand forms the game never uses).
pub fn unsupported(i: &Instruction) -> Option<String> {
    use MemorySize::*;
    if i.op_kind(0) == OpKind::Memory || i.op_kind(1) == OpKind::Memory {
        let ok = matches!(i.memory_size(), Float32 | Float64 | Float80 | Int16 | Int32 | Int64 | UInt16);
        if !ok {
            return Some(format!("{:?} {:?}", i.mnemonic(), i.memory_size()));
        }
    }
    None
}

/// The x87 registers held in Cranelift values during a straight run of instructions.
#[derive(Default)]
pub struct Cache {
    /// used in this function (not with jump tables)
    pub enabled: bool,
    /// TOP minus TOP at the run's start
    d: i64,
    /// by place relative to TOP at the run's start: the value, and whether memory lacks it
    vals: [Option<(Value, bool)>; 8],
    /// the control word (I16; memory has it too): one value along the run, so the rounding
    /// mode's tests are made once, not at every operation
    cw: Option<Value>,
    /// the control word's tests for the arithmetic (`Mode`), made where the run is
    mode: Option<Mode>,
}

/// What the control word says for the arithmetic's rounding.
#[derive(Clone, Copy)]
struct Mode {
    /// I32 control word
    cw: Value,
    /// to nearest with 53 or 64 bits: doubles as they are
    usual: Value,
    /// 24 bits to nearest or truncating
    near_or_trunc: Value,
    /// what to add before cutting to 24 bits (half a unit to nearest, else 0)
    add: Value,
}

impl Lifter<'_, '_> {
    /// Write the cached registers back to `Cpu::st` and forget them.
    pub(super) fn x87_settle(&mut self) {
        let d = self.xc.d;
        for r in 0..8i64 {
            if let Some((v, true)) = self.xc.vals[r as usize] {
                // place r is ST((r - d) & 7) now
                let p = self.slot(((r - d) & 7) as u32);
                self.b.ins().store(mf(), v, p, offset_of!(Cpu, st) as i32);
            }
        }
        self.xc.vals = [None; 8];
        self.xc.d = 0;
        self.xc.cw = None;
        self.xc.mode = None;
        // the end of a run's 0xc7f version: the SSE rounding as it was
        if let Some(old) = self.rz.take() {
            self.b.ins().call(self.refs.mxcsr_set, &[old]);
        }
    }

    fn mode(&mut self) -> Mode {
        if let Some(m) = self.xc.mode.filter(|_| self.xc.enabled) {
            return m;
        }
        let cw = self.fcw();
        let cw = self.b.ins().uextend(I32, cw);
        // rounding control 00 (to nearest) and precision control 1x (53 or 64 bits)
        let m = self.b.ins().band_imm_u(cw, 0xe00);
        let usual = self.b.ins().icmp_imm_u(IntCC::Equal, m, 0x200);
        let pcrc = self.b.ins().band_imm_u(cw, 0xf00);
        let trunc = self.b.ins().icmp_imm_u(IntCC::Equal, pcrc, 0xc00);
        let near = self.b.ins().icmp_imm_u(IntCC::Equal, pcrc, 0);
        let near_or_trunc = self.b.ins().bor(near, trunc);
        let half = self.iconst(I64, 1 << 28);
        let zero = self.iconst(I64, 0);
        let add = self.b.ins().select(near, half, zero);
        // stored (nothing reads them): made here once, not again in every operation's block
        // (Cranelift makes a pure value where it is first used; the operations' blocks do not
        // dominate each other)
        let flag = self.b.ins().uextend(I64, near_or_trunc);
        let at = offset_of!(Cpu, x87_mode) as i32;
        self.b.ins().store(mf(), add, self.cpu, at);
        self.b.ins().store(mf(), flag, self.cpu, at + 8);
        let m = Mode { cw, usual, near_or_trunc, add };
        self.xc.mode = Some(m);
        m
    }

    /// The control word (I16).
    pub(super) fn fcw(&mut self) -> Value {
        if let Some(v) = self.xc.cw.filter(|_| self.xc.enabled) {
            return v;
        }
        let v = self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fcw) as i32);
        self.xc.cw = Some(v);
        v
    }

    fn place(&self, k: u32) -> usize {
        ((self.xc.d + k as i64) & 7) as usize
    }

    // ---- the register stack --------------------------------------------------------------
    /// Host pointer to the physical slot of ST(k), as a byte offset into the 8-entry arrays.
    fn slot(&mut self, k: u32) -> Value {
        let t = self.b.use_var(self.top);
        let t = if k == 0 { t } else { self.b.ins().iadd_imm_s(t, k as i64) };
        let t = self.b.ins().band_imm_u(t, 7);
        let t = self.b.ins().uextend(I64, t);
        let t = self.b.ins().ishl_imm_u(t, 3);
        self.b.ins().iadd(self.cpu, t)
    }
    fn st(&mut self, k: u32) -> Value {
        if self.xc.enabled {
            if let Some((v, _)) = self.xc.vals[self.place(k)] {
                return v;
            }
        }
        let p = self.slot(k);
        let v = self.b.ins().load(F64, mf(), p, offset_of!(Cpu, st) as i32);
        if self.xc.enabled {
            let r = self.place(k);
            self.xc.vals[r] = Some((v, false));
        }
        v
    }
    fn set_st(&mut self, k: u32, v: Value) {
        if self.xc.enabled {
            let r = self.place(k);
            self.xc.vals[r] = Some((v, true));
            return;
        }
        let p = self.slot(k);
        self.b.ins().store(mf(), v, p, offset_of!(Cpu, st) as i32);
    }
    fn add_top(&mut self, d: i64) {
        self.xc.d += d;
        let t = self.b.use_var(self.top);
        let t = self.b.ins().iadd_imm_s(t, d);
        let t = self.b.ins().band_imm_u(t, 7);
        self.b.def_var(self.top, t);
    }
    fn fpush(&mut self, v: Value) {
        self.add_top(-1);
        self.set_st(0, v);
    }
    fn fpop(&mut self) {
        self.add_top(1);
    }

    /// Copy ST(src) with its exact integer (see `Cpu::st_int`) to ST(dst). Cached: the value only.
    fn copy_st(&mut self, dst: u32, src: u32) {
        if self.xc.enabled {
            let v = self.st(src);
            self.set_st(dst, v);
            return;
        }
        let (ps, pd) = (self.slot(src), self.slot(dst));
        for (off, t) in [(offset_of!(Cpu, st), F64), (offset_of!(Cpu, st_int), I64), (offset_of!(Cpu, st_int_of), I64)] {
            let v = self.b.ins().load(t, mf(), ps, off as i32);
            self.b.ins().store(mf(), v, pd, off as i32);
        }
    }

    fn fsw(&mut self) -> Value {
        self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fsw) as i32)
    }
    fn set_fsw(&mut self, v: Value) {
        self.b.ins().store(mf(), v, self.cpu, offset_of!(Cpu, fsw) as i32);
    }

    // ---- memory operands -------------------------------------------------------------------
    /// The memory operand as a double (float or integer, by its size).
    fn fmem(&mut self, i: &Instruction) -> Value {
        let a = self.addr(i);
        match i.memory_size() {
            MemorySize::Float32 => {
                let v = self.load_t(a, F32);
                self.b.ins().fpromote(F64, v)
            }
            MemorySize::Float64 => self.load_t(a, F64),
            MemorySize::Int16 => {
                let v = self.load(a, 2);
                let v = self.b.ins().sextend(I32, v);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            MemorySize::Int32 => {
                let v = self.load(a, 4);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            MemorySize::Int64 => {
                let v = self.load(a, 8);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            s => panic!("x87 memory operand {s:?}"),
        }
    }

    fn load_t(&mut self, a: Value, t: cranelift_codegen::ir::Type) -> Value {
        let p = self.host(a);
        self.b.ins().load(t, mf(), p, 0)
    }

    /// Run `rt_x87` on the synced state.
    fn x87_helper(&mut self, op: X87, addr: Option<Value>) {
        self.x87_settle();
        self.store_state();
        let op = self.iconst(I32, op as i64);
        let a = addr.unwrap_or_else(|| self.iconst(I32, 0));
        self.b.ins().call(self.refs.x87, &[self.cpu, op, a]);
        self.load_state();
    }

    // ---- conversions -----------------------------------------------------------------------
    /// Round to an integer as the control word's rounding mode says.
    fn round_rc(&mut self, v: Value) -> Value {
        let cw = self.fcw();
        let rc = self.b.ins().ushr_imm_u(cw, 10);
        let rc = self.b.ins().band_imm_u(rc, 3);
        let near = self.b.ins().nearest(v);
        let down = self.b.ins().floor(v);
        let up = self.b.ins().ceil(v);
        let trunc = self.b.ins().trunc(v);
        let (is1, is2, is3) = (
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 1),
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 2),
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 3),
        );
        let r = self.b.ins().select(is1, down, near);
        let r = self.b.ins().select(is2, up, r);
        self.b.ins().select(is3, trunc, r)
    }

    /// A double as a `size`-byte integer, rounded by the control word (`truncate` for
    /// fisttp); out of range or NaN gives the "integer indefinite" (the smallest integer).
    fn to_int(&mut self, v: Value, size: u32, truncate: bool) -> Value {
        let r = if truncate { self.b.ins().trunc(v) } else { self.round_rc(v) };
        let lim = (1u64 << (8 * size - 1)) as f64;
        let (lo, hi) = (self.b.ins().f64const(-lim), self.b.ins().f64const(lim));
        let ge = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, r, lo);
        let lt = self.b.ins().fcmp(FloatCC::LessThan, r, hi);
        let fits = self.b.ins().band(ge, lt);
        let x = self.b.ins().fcvt_to_sint_sat(if size == 8 { I64 } else { I32 }, r);
        let x = if size == 2 { self.b.ins().ireduce(I16, x) } else { x };
        let min = self.iconst(ty(size), (1u64 << (8 * size - 1)) as i64);
        self.b.ins().select(fits, x, min)
    }

    /// x op y as the FPU rounds it. Doubles to nearest when the control word asks for 53
    /// or 64 bits to nearest. 24 bits to nearest or truncating (the renderer's 0xc7f) are
    /// done here too (`round24`); anything else goes to `rt_fpu_round`.
    fn arith(&mut self, op: FpuOp, x: Value, y: Value) -> Value {
        if self.rz.is_some() {
            return self.arith_rz(op, x, y);
        }
        let fast = match op {
            FpuOp::Add => self.b.ins().fadd(x, y),
            FpuOp::Sub => self.b.ins().fsub(x, y),
            FpuOp::Mul => self.b.ins().fmul(x, y),
            FpuOp::Div => self.b.ins().fdiv(x, y),
            FpuOp::Sqrt => self.b.ins().sqrt(x),
            FpuOp::Round => self.b.ins().fdemote(F32, x),
        };
        // the double nearest the exact result (for a store: x itself)
        let s = if matches!(op, FpuOp::Round) { x } else { fast };
        let fast = if matches!(op, FpuOp::Round) { self.b.ins().fpromote(F64, fast) } else { fast };
        let md = self.mode();
        let (cw, usual) = (md.cw, md.usual);
        let (slow, edge, call_blk, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, F64);
        self.b.ins().brif(usual, done, &[fast.into()], slow, &[]);
        self.b.switch_to_block(slow);
        let (ok, r) = self.round24_between(op, x, y, s, md);
        self.b.ins().brif(ok, done, &[r.into()], edge, &[]);
        self.b.switch_to_block(edge);
        let (ok, r) = self.round24(op, x, y, s, cw);
        self.b.ins().brif(ok, done, &[r.into()], call_blk, &[]);
        self.b.switch_to_block(call_blk);
        let o = self.iconst(I32, op as i64);
        let call = self.b.ins().call(self.refs.fpu_round, &[o, x, y, cw]);
        let r = self.b.inst_results(call)[0];
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// x op y under the control word 0xc7f, in a run's version where the SSE rounding
    /// truncates too (`Lifter::versions`): the double result is the exact one truncated, and
    /// truncating it to 24 bits gives the FPU's result, if it is a normal double or an exact
    /// zero; else `rt_fpu_round`. The result's test is the condition of a branch: the operation
    /// is made there, after the SSE mode changed, not moved elsewhere by Cranelift.
    fn arith_rz(&mut self, op: FpuOp, x: Value, y: Value) -> Value {
        const UNIT: i64 = 1 << 29;
        let s = match op {
            FpuOp::Add => self.b.ins().fadd(x, y),
            FpuOp::Sub => self.b.ins().fsub(x, y),
            FpuOp::Mul => self.b.ins().fmul(x, y),
            FpuOp::Div => self.b.ins().fdiv(x, y),
            FpuOp::Sqrt => self.b.ins().sqrt(x),
            FpuOp::Round => unreachable!("a store under 0xc7f is fdemote"),
        };
        let bits = self.b.ins().bitcast(I64, mf(), s);
        let mag = self.b.ins().band_imm_u(bits, i64::MAX);
        // (not the largest double: truncating, an overflow gives it)
        let from_lo = self.b.ins().iadd_imm_s(mag, -0x0010_0000_0000_0000);
        let normal = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, from_lo, 0x7fdf_ffff_ffff_ffff);
        // cut in a vector register (see `round24_between`)
        let le = mf().with_endianness(Endianness::Little);
        let v = self.b.ins().scalar_to_vector(F64X2, s);
        let v = self.b.ins().bitcast(I64X2, le, v);
        let mask = self.b.func.dfg.constants.insert((!(UNIT - 1)).to_le_bytes().repeat(2).into());
        let mask = self.b.ins().vconst(I64X2, mask);
        let v = self.b.ins().band(v, mask);
        let v = self.b.ins().bitcast(F64X2, le, v);
        let r = self.b.ins().extractlane(v, 0);
        let (other, call_blk, done) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, F64);
        self.b.ins().brif(normal, done, &[r.into()], other, &[]);
        // zero: exact unless a product or quotient of nonzero numbers came out below the
        // doubles' range (a sum of doubles is zero only if exact)
        self.b.switch_to_block(other);
        let zero = self.b.ins().icmp_imm_u(IntCC::Equal, mag, 0);
        let f0 = self.b.ins().f64const(0.0);
        let exact = match op {
            FpuOp::Mul => {
                let (xz, yz) = (self.b.ins().fcmp(FloatCC::Equal, x, f0), self.b.ins().fcmp(FloatCC::Equal, y, f0));
                self.b.ins().bor(xz, yz)
            }
            FpuOp::Div => self.b.ins().fcmp(FloatCC::Equal, x, f0),
            _ => self.iconst(I8, 1),
        };
        let zero = self.b.ins().band(zero, exact);
        self.b.ins().brif(zero, done, &[s.into()], call_blk, &[]);
        self.b.switch_to_block(call_blk);
        let cw = self.fcw();
        let cw = self.b.ins().uextend(I32, cw);
        let o = self.iconst(I32, op as i64);
        let call = self.b.ins().call(self.refs.fpu_round, &[o, x, y, cw]);
        let r = self.b.inst_results(call)[0];
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    /// The usual cases of `round24`: s between two 24-bit numbers and not in their middle (its
    /// low 29 bits neither 0 nor 0x10000000), or on one without an error; normal (for a store:
    /// in a float's normal range) or an exact zero. (done, the result.)
    fn round24_between(&mut self, op: FpuOp, x: Value, y: Value, s: Value, md: Mode) -> (Value, Value) {
        const UNIT: i64 = 1 << 29;
        const HALF: i64 = 1 << 28;
        let bits = self.b.ins().bitcast(I64, mf(), s);
        let mode = md.near_or_trunc;
        let low = self.b.ins().band_imm_u(bits, UNIT - 1);
        let off = self.b.ins().icmp_imm_u(IntCC::NotEqual, low, 0);
        let not_mid = self.b.ins().icmp_imm_u(IntCC::NotEqual, low, HALF);
        // or on a 24-bit number without an error: s is the result. The game's results are
        // often exact (integers times simple factors, stores of 24-bit results): ~20 million
        // operations a second went to `round24` (2026-11-08). The error is a branch's
        // condition only, not on the way of the result.
        // (a store has no error)
        let exact = (!matches!(op, FpuOp::Round)).then(|| {
            let e = self.error(op, x, y, s);
            let zero = self.b.ins().f64const(0.0);
            self.b.ins().fcmp(FloatCC::Equal, e, zero)
        });
        let between = match exact {
            Some(exact) => {
                let off_or_exact = self.b.ins().bor(off, exact);
                self.b.ins().band(off_or_exact, not_mid)
            }
            None => not_mid,
        };
        // the magnitude's bits: a normal double (or a float's normal range for a store)
        let mag = self.b.ins().band_imm_u(bits, i64::MAX);
        let (lo, hi) = if matches!(op, FpuOp::Round) { (0x3810_0000_0000_0000, 0x47f0_0000_0000_0000) } else { (0x0010_0000_0000_0000, 0x7ff0_0000_0000_0000) };
        let from_lo = self.b.ins().iadd_imm_s(mag, -lo);
        let range = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, from_lo, hi - lo);
        // or an exact zero (x * 0: ~2 million a second)
        let zero = self.b.ins().icmp_imm_u(IntCC::Equal, mag, 0);
        let zero = match exact {
            Some(exact) => self.b.ins().band(zero, exact),
            None => zero,
        };
        let range = self.b.ins().bor(range, zero);
        let ok = self.b.ins().band(mode, between);
        let ok = self.b.ins().band(ok, range);
        // to nearest: add half a unit, then cut. In a vector register: the result is the next
        // operation's operand, and moving it to an integer register and back made each
        // operation of a chain ~8 cycles slower (the check above is only a branch's condition)
        let le = mf().with_endianness(Endianness::Little);
        let v = self.b.ins().scalar_to_vector(F64X2, s);
        let v = self.b.ins().bitcast(I64X2, le, v);
        let add = self.b.ins().scalar_to_vector(I64X2, md.add);
        let v = self.b.ins().iadd(v, add);
        let mask = self.b.func.dfg.constants.insert((!(UNIT - 1)).to_le_bytes().repeat(2).into());
        let mask = self.b.ins().vconst(I64X2, mask);
        let v = self.b.ins().band(v, mask);
        let v = self.b.ins().bitcast(F64X2, le, v);
        let r = self.b.ins().extractlane(v, 0);
        (ok, r)
    }

    /// The error e = exact - s of x op y (s: the double nearest the exact result); its sign
    /// is what counts, and whether it is 0.
    fn error(&mut self, op: FpuOp, x: Value, y: Value, s: Value) -> Value {
        match op {
            FpuOp::Add | FpuOp::Sub => {
                // TwoSum
                let y = if matches!(op, FpuOp::Sub) { self.b.ins().fneg(y) } else { y };
                let b = self.b.ins().fsub(s, x);
                let sb = self.b.ins().fsub(s, b);
                let ex = self.b.ins().fsub(x, sb);
                let ey = self.b.ins().fsub(y, b);
                self.b.ins().fadd(ex, ey)
            }
            FpuOp::Mul => {
                let ns = self.b.ins().fneg(s);
                self.b.ins().fma(x, y, ns)
            }
            FpuOp::Div => {
                // x - s*y; the quotient's error has its sign times y's
                let ns = self.b.ins().fneg(s);
                let r = self.b.ins().fma(ns, y, x);
                let rb = self.b.ins().bitcast(I64, mf(), r);
                let yb = self.b.ins().bitcast(I64, mf(), y);
                let ys = self.b.ins().band_imm_u(yb, i64::MIN);
                let eb = self.b.ins().bxor(rb, ys);
                self.b.ins().bitcast(F64, mf(), eb)
            }
            FpuOp::Sqrt => {
                let ns = self.b.ins().fneg(s);
                self.b.ins().fma(ns, s, x)
            }
            FpuOp::Round => self.b.ins().f64const(0.0),
        }
    }

    /// x op y rounded to 24 bits, to nearest or truncating, from s (the double nearest the
    /// exact result) and the sign of s's error: (done, the result). Not done: another control
    /// word, a subnormal, zero with an error, infinite or NaN (left to `rt_fpu_round`).
    /// The exact result and s lie on the same side of every 24-bit number and of every middle
    /// between two, unless s is one of them (its low 29 bits 0, or 0x10000000): then the
    /// error's sign decides. Mostly the game's results are 24-bit numbers themselves.
    fn round24(&mut self, op: FpuOp, x: Value, y: Value, s: Value, cw: Value) -> (Value, Value) {
        const UNIT: i64 = 1 << 29;
        const HALF: i64 = 1 << 28;
        let e = self.error(op, x, y, s);
        let fzero = self.b.ins().f64const(0.0);
        let exact = self.b.ins().fcmp(FloatCC::Equal, e, fzero);
        let bits = self.b.ins().bitcast(I64, mf(), s);
        let eb = self.b.ins().bitcast(I64, mf(), e);
        // e has the sign of s: the exact result is farther from zero
        let signs = self.b.ins().bxor(bits, eb);
        let away = self.b.ins().icmp_imm_u(IntCC::SignedGreaterThanOrEqual, signs, 0);
        let pcrc = self.b.ins().band_imm_u(cw, 0xf00);
        let trunc = self.b.ins().icmp_imm_u(IntCC::Equal, pcrc, 0xc00);
        let near = self.b.ins().icmp_imm_u(IntCC::Equal, pcrc, 0);
        let low = self.b.ins().band_imm_u(bits, UNIT - 1);
        let on = self.b.ins().icmp_imm_u(IntCC::Equal, low, 0);
        let mid = self.b.ins().icmp_imm_u(IntCC::Equal, low, HALF);
        let lo = self.b.ins().band_imm_u(bits, !(UNIT - 1));
        let hi = self.b.ins().iadd_imm_s(lo, UNIT);
        // s between two 24-bit numbers: to nearest the nearer one, truncating the lower
        let up = self.b.ins().iadd_imm_s(bits, HALF);
        let up = self.b.ins().band_imm_u(up, !(UNIT - 1));
        let between = self.b.ins().select(near, up, lo);
        // s on a 24-bit number: itself, or truncating the one below if the exact result is
        // nearer zero
        let keep = self.b.ins().bor(near, exact);
        let keep = self.b.ins().bor(keep, away);
        let below = self.b.ins().iadd_imm_s(bits, -UNIT);
        let on_r = self.b.ins().select(keep, bits, below);
        // s in the middle, to nearest: by the error, a tie to the even one
        let odd = self.b.ins().band_imm_u(lo, UNIT);
        let lo_even = self.b.ins().icmp_imm_u(IntCC::Equal, odd, 0);
        let even = self.b.ins().select(lo_even, lo, hi);
        let side = self.b.ins().select(away, hi, lo);
        let mid_r = self.b.ins().select(exact, even, side);
        let use_mid = self.b.ins().band(near, mid);
        let r = self.b.ins().select(use_mid, mid_r, between);
        let r = self.b.ins().select(on, on_r, r);
        let r = self.b.ins().bitcast(F64, mf(), r);
        // finite, normal (a subnormal double keeps fewer than the 24 bits the FPU's wider
        // exponent keeps), or an exact zero
        let exp = self.b.ins().band_imm_u(bits, 0x7ff0_0000_0000_0000);
        let normal = self.b.ins().icmp_imm_u(IntCC::NotEqual, exp, 0);
        let finite = self.b.ins().icmp_imm_u(IntCC::NotEqual, exp, 0x7ff0_0000_0000_0000);
        let normal = self.b.ins().band(normal, finite);
        let mag = self.b.ins().band_imm_u(bits, i64::MAX);
        let zero = self.b.ins().icmp_imm_u(IntCC::Equal, mag, 0);
        let zero = self.b.ins().band(zero, exact);
        let normal = if matches!(op, FpuOp::Round) {
            // a float's own range: below it a store keeps fewer bits, above it infinity
            let lo = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, mag, 0x3810_0000_0000_0000);
            let hi = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, mag, 0x47f0_0000_0000_0000);
            let f32_range = self.b.ins().band(lo, hi);
            self.b.ins().band(normal, f32_range)
        } else {
            normal
        };
        let fine = self.b.ins().bor(normal, zero);
        let mode = self.b.ins().bor(near, trunc);
        let ok = self.b.ins().band(mode, fine);
        // e NaN (an infinite operand): not exact, so leave it
        let e_nan = self.b.ins().fcmp(FloatCC::Unordered, e, e);
        let ok_e = self.b.ins().bnot(e_nan);
        let ok = self.b.ins().band(ok, ok_e);
        (ok, r)
    }

    // ---- comparisons -----------------------------------------------------------------------
    /// fcom and friends: C3 C2 C0 = 000 (a > b), 001 (a < b), 100 (equal), 111 (unordered).
    fn compare_fsw(&mut self, a: Value, b: Value) {
        let bit = |l: &mut Self, cc: FloatCC, mask: i64| {
            let c = l.b.ins().fcmp(cc, a, b);
            let c = l.b.ins().uextend(I16, c);
            l.b.ins().imul_imm_u(c, mask)
        };
        let c0 = bit(self, FloatCC::UnorderedOrLessThan, C0);
        let c2 = bit(self, FloatCC::Unordered, C2);
        let c3 = bit(self, FloatCC::UnorderedOrEqual, C3);
        let sw = self.fsw();
        let sw = self.b.ins().band_imm_u(sw, 0xffff & !(C0 | C2 | C3 | 0x200));
        let sw = self.b.ins().bor(sw, c0);
        let sw = self.b.ins().bor(sw, c2);
        let sw = self.b.ins().bor(sw, c3);
        self.set_fsw(sw);
    }

    /// fcomi and friends set ZF, PF, CF like an unsigned integer compare (all three if unordered).
    fn compare_eflags(&mut self, a: Value, b: Value) {
        let z = self.b.ins().fcmp(FloatCC::UnorderedOrEqual, a, b);
        let p = self.b.ins().fcmp(FloatCC::Unordered, a, b);
        let c = self.b.ins().fcmp(FloatCC::UnorderedOrLessThan, a, b);
        self.set_flag(self.zf, z);
        self.set_flag(self.pf, p);
        self.set_flag(self.cf, c);
        self.zero_flags(&[self.of, self.sf]);
    }

    // ---- instructions ----------------------------------------------------------------------
    pub(super) fn x87(&mut self, i: &Instruction) {
        use Mnemonic::*;
        let m = i.mnemonic();
        let sti = |k: u32| i.op_register(k).number() as u32 - Register::ST0.number() as u32;
        let mem0 = i.op_count() > 0 && i.op0_kind() == OpKind::Memory;
        match m {
            Fld | Fild => {
                if mem0 && i.memory_size() == MemorySize::Float80 {
                    let a = self.addr(i);
                    self.x87_helper(X87::LoadF80, Some(a));
                } else if mem0 {
                    let v = self.fmem(i);
                    self.fpush(v);
                    if i.memory_size() == MemorySize::Int64 {
                        // remember the integer: fistp qword of this double gives it back
                        let a = self.addr(i);
                        let x = self.load(a, 8);
                        let p = self.slot(0);
                        self.b.ins().store(mf(), x, p, offset_of!(Cpu, st_int) as i32);
                        let bits = self.b.ins().bitcast(I64, mf(), v);
                        self.b.ins().store(mf(), bits, p, offset_of!(Cpu, st_int_of) as i32);
                    }
                } else {
                    // fld st(i): push a copy (ST(i) is ST(i+1) after the push)
                    let k = sti(0);
                    self.add_top(-1);
                    self.copy_st(0, k + 1);
                }
            }
            Fst | Fstp => {
                if mem0 && i.memory_size() == MemorySize::Float80 {
                    let a = self.addr(i);
                    self.x87_helper(X87::StoreF80, Some(a));
                } else if mem0 {
                    let v = self.st(0);
                    let a = self.addr(i);
                    let v = if i.memory_size() == MemorySize::Float32 && self.rz.is_some() {
                        // the SSE rounding truncates as the control word says
                        self.b.ins().fdemote(F32, v)
                    } else if i.memory_size() == MemorySize::Float32 {
                        // rounded to a float as the control word says
                        let v = self.arith(FpuOp::Round, v, v);
                        self.b.ins().fdemote(F32, v)
                    } else {
                        v
                    };
                    self.store(a, v);
                } else {
                    let k = sti(0);
                    if k != 0 {
                        self.copy_st(k, 0);
                    }
                }
                if m == Fstp && !(mem0 && i.memory_size() == MemorySize::Float80) {
                    self.fpop();
                }
            }
            Fist | Fistp | Fisttp => {
                let n = i.memory_size().size() as u32;
                let v = self.st(0);
                let mut x = self.to_int(v, n, m == Fisttp);
                if n == 8 {
                    // the exact integer, if this double came from a fild qword
                    let p = self.slot(0);
                    let bits = self.b.ins().bitcast(I64, mf(), v);
                    let of = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st_int_of) as i32);
                    let exact = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st_int) as i32);
                    let same = self.b.ins().icmp(IntCC::Equal, bits, of);
                    x = self.b.ins().select(same, exact, x);
                }
                let a = self.addr(i);
                self.store(a, x);
                if m != Fist {
                    self.fpop();
                }
            }
            Fxch if self.xc.enabled => {
                let k = sti(1);
                let (a, b) = (self.st(0), self.st(k));
                self.set_st(0, b);
                self.set_st(k, a);
            }
            Fxch => {
                let k = sti(1); // fxch st(0), st(i)
                let (p0, pk) = (self.slot(0), self.slot(k));
                for (off, t) in [(offset_of!(Cpu, st), F64), (offset_of!(Cpu, st_int), I64), (offset_of!(Cpu, st_int_of), I64)] {
                    let a = self.b.ins().load(t, mf(), p0, off as i32);
                    let b = self.b.ins().load(t, mf(), pk, off as i32);
                    self.b.ins().store(mf(), b, p0, off as i32);
                    self.b.ins().store(mf(), a, pk, off as i32);
                }
            }
            Fadd | Faddp | Fiadd | Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr | Fmul | Fmulp | Fimul | Fdiv | Fdivp
            | Fidiv | Fdivr | Fdivrp | Fidivr => {
                // dst = dst op src (or src op dst for the reversed forms)
                let (dst, src) = if mem0 {
                    (0, self.fmem(i))
                } else if i.op_count() == 0 {
                    (1, self.st(0)) // faddp with no operands: st(1), st(0)
                } else {
                    let (d, s) = (sti(0), sti(1));
                    (d, self.st(s))
                };
                let d = self.st(dst);
                let rev = matches!(m, Fsubr | Fsubrp | Fisubr | Fdivr | Fdivrp | Fidivr);
                let (x, y) = if rev { (src, d) } else { (d, src) };
                let op = match m {
                    Fadd | Faddp | Fiadd => FpuOp::Add,
                    Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr => FpuOp::Sub,
                    Fmul | Fmulp | Fimul => FpuOp::Mul,
                    _ => FpuOp::Div,
                };
                let r = self.arith(op, x, y);
                self.set_st(dst, r);
                if matches!(m, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp) {
                    self.fpop();
                }
            }
            Fchs | Fabs | Fsqrt | Frndint => {
                let v = self.st(0);
                let r = match m {
                    Fchs => self.b.ins().fneg(v),
                    Fabs => self.b.ins().fabs(v),
                    Fsqrt => self.arith(FpuOp::Sqrt, v, v),
                    _ => self.round_rc(v),
                };
                self.set_st(0, r);
            }
            Fld1 | Fldz | Fldpi | Fldl2e | Fldl2t | Fldlg2 | Fldln2 => {
                use std::f64::consts::*;
                let c = match m {
                    Fld1 => 1.0,
                    Fldz => 0.0,
                    Fldpi => PI,
                    Fldl2e => LOG2_E,
                    Fldl2t => LOG2_10,
                    Fldlg2 => LOG10_2,
                    _ => LN_2,
                };
                let v = self.b.ins().f64const(c);
                self.fpush(v);
            }
            Fcom | Fcomp | Fucom | Fucomp | Ficom | Ficomp | Fcompp | Fucompp | Ftst => {
                let a = self.st(0);
                let b = if m == Ftst {
                    self.b.ins().f64const(0.0)
                } else if mem0 {
                    self.fmem(i)
                } else if i.op_count() == 0 {
                    self.st(1)
                } else {
                    let k = sti(i.op_count() - 1);
                    self.st(k)
                };
                self.compare_fsw(a, b);
                let pops = match m {
                    Fcompp | Fucompp => 2,
                    Fcomp | Fucomp | Ficomp => 1,
                    _ => 0,
                };
                for _ in 0..pops {
                    self.fpop();
                }
            }
            Fcomi | Fcomip | Fucomi | Fucomip => {
                let a = self.st(0);
                let b = self.st(sti(1));
                self.compare_eflags(a, b);
                if matches!(m, Fcomip | Fucomip) {
                    self.fpop();
                }
            }
            Fnstsw => {
                let sw = self.fsw();
                let sw = self.b.ins().band_imm_u(sw, 0xffff & !0x3800);
                let t = self.b.use_var(self.top);
                let t = self.b.ins().ireduce(I16, t);
                let t = self.b.ins().ishl_imm_u(t, 11);
                let v = self.b.ins().bor(sw, t);
                self.wr(i, 0, v);
            }
            Fnstcw => {
                let v = self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fcw) as i32);
                self.wr(i, 0, v);
            }
            Fldcw => {
                let v = self.rd(i, 0, 2);
                self.b.ins().store(mf(), v, self.cpu, offset_of!(Cpu, fcw) as i32);
                self.xc.cw = Some(v);
                self.xc.mode = None;
            }
            Fnclex => {
                let sw = self.fsw();
                let sw = self.b.ins().band_imm_u(sw, 0x7f00);
                self.set_fsw(sw);
            }
            Fninit => {
                self.x87_settle();
                let cw = self.iconst(I16, 0x37f);
                self.b.ins().store(mf(), cw, self.cpu, offset_of!(Cpu, fcw) as i32);
                self.xc.cw = Some(cw);
                self.xc.mode = None;
                let z = self.iconst(I16, 0);
                self.set_fsw(z);
                let z = self.iconst(I32, 0);
                self.b.def_var(self.top, z);
            }
            Fincstp => self.fpop(),
            Fdecstp => self.add_top(-1),
            Ffree | Fnop => {}
            _ => {
                let op = match m {
                    Fxam => X87::Fxam,
                    Fsin => X87::Fsin,
                    Fcos => X87::Fcos,
                    Fsincos => X87::Fsincos,
                    Fptan => X87::Fptan,
                    Fpatan => X87::Fpatan,
                    F2xm1 => X87::F2xm1,
                    Fyl2x => X87::Fyl2x,
                    Fyl2xp1 => X87::Fyl2xp1,
                    Fscale => X87::Fscale,
                    Fprem => X87::Fprem,
                    Fprem1 => X87::Fprem1,
                    Fxtract => X87::Fxtract,
                    _ => panic!("{m:?} passed the support check but has no lifting"),
                };
                self.x87_helper(op, None);
            }
        }
    }
}
