//! Finding the code: every function's instructions (recursive descent from its entry,
//! following branches and switch tables), plus extra entry points for every place other
//! code jumps or calls into the middle of, and for code addresses stored in data.

use crate::image::Image;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// BEGTEXT, the code segment (its last 0x748 bytes are DirectInput's data formats)
pub const TEXT_START: u32 = 0x410000;
pub const TEXT_END: u32 = 0x4c0f58;

pub fn is_code(a: u32) -> bool {
    (TEXT_START..TEXT_END).contains(&a)
}

/// Decoding depends only on the address, so every instruction is decoded once.
pub struct Code<'a> {
    pub img: &'a Image,
    cache: HashMap<u32, Option<Instruction>>,
}

impl<'a> Code<'a> {
    pub fn new(img: &'a Image) -> Self {
        Code { img, cache: HashMap::new() }
    }

    pub fn at(&mut self, pc: u32) -> Option<Instruction> {
        let img = self.img;
        *self.cache.entry(pc).or_insert_with(|| {
            let bytes = img.read(pc, 16)?;
            let i = Decoder::with_ip(32, &bytes, pc as u64, DecoderOptions::NONE).decode();
            (!i.is_invalid()).then_some(i)
        })
    }
}

/// A direct branch or call target (jmp/jcc/loop/jecxz/call rel).
pub fn direct_target(i: &Instruction) -> Option<u32> {
    matches!(i.op0_kind(), OpKind::NearBranch32 | OpKind::NearBranch16).then(|| i.near_branch_target() as u32)
}

pub struct Func {
    pub entry: u32,
    pub lo: u32,
    pub hi: u32,
    pub name: String,
    pub ins: BTreeSet<u32>,
    /// jmp [index*4 + table]: the instruction's address -> its targets
    pub tables: HashMap<u32, Vec<u32>>,
    pub targets: HashSet<u32>,
    pub calls: HashSet<u32>,
}

impl Func {
    pub fn inside(&self, a: u32) -> bool {
        self.lo <= a && a < self.hi
    }

    fn new(code: &mut Code, entry: u32, lo: u32, hi: u32, name: String) -> Func {
        let mut f = Func {
            entry,
            lo,
            hi,
            name,
            ins: BTreeSet::new(),
            tables: HashMap::new(),
            targets: HashSet::new(),
            calls: HashSet::new(),
        };
        f.decode(code);
        f
    }

    fn decode(&mut self, code: &mut Code) {
        let mut todo = vec![self.entry];
        while let Some(mut pc) = todo.pop() {
            if !self.inside(pc) {
                continue;
            }
            while !self.ins.contains(&pc) {
                if !self.inside(pc) {
                    // runs on past the end: needs an entry there
                    self.targets.insert(pc);
                    break;
                }
                let Some(i) = code.at(pc) else { break };
                self.ins.insert(pc);
                let fc = i.flow_control();
                match fc {
                    FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch if direct_target(&i).is_some() => {
                        let t = direct_target(&i).unwrap();
                        self.targets.insert(t);
                        if self.inside(t) {
                            todo.push(t);
                        }
                        if fc == FlowControl::UnconditionalBranch {
                            break;
                        }
                    }
                    FlowControl::IndirectBranch | FlowControl::UnconditionalBranch => {
                        if i.op0_kind() == OpKind::Memory
                            && i.memory_index() != Register::None
                            && i.memory_index_scale() == 4
                            && i.memory_base() == Register::None
                        {
                            // "neg ecx; jmp [ecx*4 + T]" (memcpy's tail copy) only reads entries before T
                            let neg = self.ins.contains(&(pc.wrapping_sub(2)))
                                && code.at(pc - 2).is_some_and(|p| {
                                    p.mnemonic() == Mnemonic::Neg
                                        && p.op0_kind() == OpKind::Register
                                        && p.op0_register() == i.memory_index()
                                });
                            let tbl = self.table(code, i.memory_displacement32(), if neg { 8 } else { 4 });
                            for &t in &tbl {
                                self.targets.insert(t);
                                if self.inside(t) {
                                    todo.push(t);
                                }
                            }
                            self.tables.insert(pc, tbl);
                        }
                        break;
                    }
                    _ => match i.mnemonic() {
                        Mnemonic::Ret | Mnemonic::Retf | Mnemonic::Iretd | Mnemonic::Int3 | Mnemonic::Hlt => break,
                        Mnemonic::Call => {
                            if let Some(t) = direct_target(&i) {
                                self.calls.insert(t);
                            }
                        }
                        _ => {}
                    },
                }
                pc += i.len() as u32;
            }
        }
    }

    /// Jump table entries; MSVC's memcpy indexes some tables with negative values, so valid
    /// entries just before the table count too.
    fn table(&self, code: &Code, at: u32, back: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for k in 1..=back {
            let t = code.img.u32(at - 4 * k);
            if !self.inside(t) || self.ins.contains(&(at - 4 * k)) {
                break;
            }
            out.push(t);
        }
        // tables sometimes point into a neighbouring "function" (gap-filling splits):
        // accept any code address, foreign ones become extra entry points
        let mut fwd: Vec<u32> = Vec::new();
        loop {
            let slot = at + 4 * fwd.len() as u32;
            let mut t = code.img.u32(slot);
            if fwd.len() > 512 || (!fwd.is_empty() && self.ins.contains(&slot)) {
                break;
            }
            if !is_code(t) {
                if !fwd.is_empty() {
                    break; // an unused leading slot may hold anything
                }
                t = 0;
            }
            fwd.push(t);
        }
        out.extend(fwd.into_iter().filter(|&x| x != 0));
        out
    }
}

pub struct Program {
    /// entry -> function
    pub fns: BTreeMap<u32, Func>,
}

impl Program {
    pub fn build(code: &mut Code, ranges: &[(u32, u32, String)]) -> Program {
        let mut fns: BTreeMap<u32, Func> = BTreeMap::new();
        for (lo, hi, name) in ranges {
            fns.insert(*lo, Func::new(code, *lo, *lo, *hi, name.clone()));
        }
        // code addresses stored in data (function pointer and jump target tables the
        // self-modifying renderer uses) must be enterable too
        let mut insn_at = HashSet::new();
        let mut inside = HashSet::new(); // bytes strictly inside a decoded instruction
        for f in fns.values() {
            for &a in &f.ins {
                insn_at.insert(a);
                let len = code.at(a).unwrap().len() as u32;
                inside.extend(a + 1..a + len);
            }
        }
        let mut data_ptrs = HashSet::new();
        for s in &code.img.sections {
            if !matches!(s.name.as_str(), ".rdata" | ".data" | "DGROUP") {
                continue;
            }
            let blob = code.img.read(s.va, s.rsize as usize).unwrap_or_default();
            // Watcom's start-up tables (XI/YI: kind, priority, address) have 6-byte entries:
            // their addresses are only 2-aligned
            for k in (0..blob.len().saturating_sub(3)).step_by(2) {
                let t = u32::from_le_bytes([blob[k], blob[k + 1], blob[k + 2], blob[k + 3]]);
                let plausible = insn_at.contains(&t)
                    || (is_code(t) && !inside.contains(&t) && !matches!(code.img.byte(t), 0x00 | 0x90 | 0xCC));
                if plausible && !fns.contains_key(&t) {
                    data_ptrs.insert(t);
                }
            }
        }
        // add entries until nothing new turns up
        let los: Vec<u32> = ranges.iter().map(|r| r.0).collect();
        let mut todo: Vec<Vec<u32>> =
            fns.values().map(|f| f.targets.iter().chain(&f.calls).copied().filter(|&t| !(f.inside(t) && f.ins.contains(&t))).collect()).collect();
        todo.push(data_ptrs.into_iter().collect());
        while !todo.is_empty() {
            let mut next = Vec::new();
            let mut wanted: Vec<u32> = todo.into_iter().flatten().collect();
            wanted.sort_unstable();
            wanted.dedup();
            for t in wanted {
                if fns.contains_key(&t) || !is_code(t) {
                    continue;
                }
                let k = los.partition_point(|&lo| lo <= t);
                if k == 0 {
                    continue;
                }
                let (lo, hi, name) = &ranges[k - 1];
                let e = if t < *hi {
                    Func::new(code, t, *lo, *hi, format!("{name}+0x{:x}", t - lo))
                } else {
                    // past the last function's end: up to the next one
                    let next = los.get(k).copied().unwrap_or(TEXT_END);
                    Func::new(code, t, t, next, format!("FUN_{t:08x}"))
                };
                next.push(e.targets.iter().chain(&e.calls).copied().filter(|&x| !(e.inside(x) && e.ins.contains(&x))).collect());
                fns.insert(t, e);
            }
            todo = next;
        }
        Program { fns }
    }
}
