//! Self-modifying code: `mov [code address], x` writes into the code (Future Cop's software
//! renderer wrote immediates, displacements and branch offsets of its inner loops). Fields
//! that are exactly an operand are read from guest memory by the generated code. Instructions
//! whose other bytes change (`variants`, from a table of encodings: none known in Nuclear
//! Strike yet) get one version per encoding, chosen at run time by comparing the bytes.

use crate::discover::{Code, Program, is_code};
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, HashMap};

/// A code field written at run time: its guest address and size in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub at: u32,
    pub size: u32,
}

#[derive(Default)]
pub struct Smc {
    /// instruction address -> its immediate (for branches: the offset), read at run time
    pub imm: HashMap<u32, Field>,
    /// instruction address -> its memory displacement, read at run time
    pub disp: HashMap<u32, Field>,
    /// instruction address -> the other encodings it can be rewritten to (same length)
    pub variants: HashMap<u32, Vec<(Vec<u8>, Instruction)>>,
}

impl Smc {
    pub fn find(prog: &Program, code: &mut Code) -> Smc {
        let mut starts: Vec<u32> = prog.fns.values().flat_map(|f| f.ins.iter().copied()).collect();
        starts.sort_unstable();
        starts.dedup();

        // fields: what `mov [code address], x` writes, and the patch table's entries
        let mut fields: BTreeMap<u32, (u32, Option<[u32; 2]>)> = BTreeMap::new();
        for &a in &starts {
            let i = code.at(a).unwrap();
            if i.mnemonic() == Mnemonic::Mov
                && i.op0_kind() == OpKind::Memory
                && i.memory_base() == Register::None
                && i.memory_index() == Register::None
                && is_code(i.memory_displacement32())
            {
                fields.insert(i.memory_displacement32(), (i.memory_size().size() as u32, None));
            }
        }
        // code fields with known values: (address, size, value per variant)
        let table: Vec<(u32, u32, [u32; 2])> = Vec::new();
        for &(f, n, v) in &table {
            fields.insert(f, (n, Some(v)));
        }

        let mut smc = Smc::default();
        let mut rewritten = Vec::new();
        for (&f, &(n, vals)) in &fields {
            let k = starts.partition_point(|&a| a <= f);
            let Some(&a) = k.checked_sub(1).map(|k| &starts[k]) else { continue };
            let i = code.at(a).unwrap();
            if f >= a + i.len() as u32 {
                eprintln!("warning: code field {f:#x} is not inside a decoded instruction");
                continue;
            }
            let co = offsets(code, a);
            let off = (f - a) as usize;
            let field = Field { at: f, size: n };
            if co.has_immediate() && off == co.immediate_offset() && n as usize == co.immediate_size() {
                smc.imm.insert(a, field);
            } else if co.has_displacement() && off == co.displacement_offset() && n as usize == co.displacement_size() {
                smc.disp.insert(a, field);
            } else if vals.is_some() {
                // every instruction the field overlaps
                rewritten.extend(starts[k - 1..].iter().copied().take_while(|&s| s < f + n));
            } else {
                eprintln!("warning: code field {f:#x} is not an operand of `{i}` at {a:#x}");
            }
        }

        // the code as each pixel format's table values leave it
        let images: Vec<HashMap<u32, u8>> = (0..2)
            .map(|k| {
                let mut img = HashMap::new();
                for &(f, n, v) in &table {
                    for (j, b) in v[k].to_le_bytes().iter().take(n as usize).enumerate() {
                        img.insert(f + j as u32, *b);
                    }
                }
                img
            })
            .collect();
        rewritten.sort_unstable();
        rewritten.dedup();
        for a in rewritten {
            let i = code.at(a).unwrap();
            let orig = code.img.read(a, i.len()).unwrap();
            let mut vs: Vec<(Vec<u8>, Instruction)> = Vec::new();
            for img in &images {
                let b: Vec<u8> = orig.iter().enumerate().map(|(j, &x)| *img.get(&(a + j as u32)).unwrap_or(&x)).collect();
                if b == orig || vs.iter().any(|v| v.0 == b) {
                    continue;
                }
                let v = Decoder::with_ip(32, &b, a as u64, DecoderOptions::NONE).decode();
                assert!(!v.is_invalid() && v.len() == i.len(), "rewritten instruction at {a:#x} changes its length");
                vs.push((b, v));
            }
            smc.variants.insert(a, vs);
        }
        smc
    }

    /// Targets of rewritten branches (discovery must decode them as code too).
    pub fn variant_targets(&self) -> impl Iterator<Item = u32> + '_ {
        self.variants.values().flatten().filter_map(|(_, v)| {
            (v.flow_control() != FlowControl::Next && v.op0_kind() == OpKind::NearBranch32).then(|| v.near_branch32())
        })
    }
}

fn offsets(code: &Code, a: u32) -> iced_x86::ConstantOffsets {
    let bytes = code.img.read(a, 16).unwrap();
    let mut d = Decoder::with_ip(32, &bytes, a as u64, DecoderOptions::NONE);
    let i = d.decode();
    d.get_constant_offsets(&i)
}
