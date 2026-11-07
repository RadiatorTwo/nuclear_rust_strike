//! `NSTRIKE_SAMPLE=1`: where the game thread spends its time. A host thread stops the thread
//! that swaps Glide's buffers some 2000 times a second, reads its instruction pointer and
//! counts it for the recompiled function it is in (the nearest start below it; hooked
//! originals count for the function before them). Every 2 s the top functions are logged;
//! "host" is our own code (imports, Glide, the runtime).
use super::log;
use std::collections::HashMap;
use std::sync::OnceLock;

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NSTRIKE_SAMPLE").is_some_and(|v| v != "0"))
}

/// Start sampling the calling thread (once).
#[cfg(windows)]
pub fn start() {
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
    static STARTED: OnceLock<()> = OnceLock::new();
    if !on() || STARTED.get().is_some() {
        return;
    }
    let _ = STARTED.set(());
    let mut h: HANDLE = std::ptr::null_mut();
    unsafe { DuplicateHandle(GetCurrentProcess(), GetCurrentThread(), GetCurrentProcess(), &mut h, 0, 0, DUPLICATE_SAME_ACCESS) };
    let h = h as usize;
    std::thread::spawn(move || sample(h as HANDLE));
}

#[cfg(not(windows))]
pub fn start() {}

#[cfg(windows)]
fn sample(h: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, CONTEXT, CONTEXT_CONTROL_AMD64};
    use windows_sys::Win32::System::Threading::{ResumeThread, SuspendThread};
    let Some(d) = crate::rt::DISPATCH.get() else { return };
    let mut starts: Vec<(usize, u32)> = d.table.iter().map(|&(g, p)| (p, g)).collect();
    starts.sort_unstable();
    let (lo, hi) = (starts.first().map_or(0, |s| s.0), starts.last().map_or(0, |s| s.0) + 0x10000);
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut modules = HashMap::new();
    let mut total = 0u32;
    let mut last = std::time::Instant::now();
    log!("sample: started");
    loop {
        std::thread::sleep(std::time::Duration::from_micros(500));
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.ContextFlags = CONTEXT_CONTROL_AMD64;
        let mut caller = 0usize;
        let rip = unsafe {
            if SuspendThread(h) == u32::MAX {
                return;
            }
            let ok = GetThreadContext(h, &mut ctx);
            ResumeThread(h);
            if ok == 0 {
                log!("sample: GetThreadContext failed: {}", std::io::Error::last_os_error());
                return;
            }
            // in system code: the first return address on the stack into our own code (in
            // its .text, after a call instruction). No unwinding: its locks may be held by
            // the stopped thread.
            let (t0, t1) = own_text();
            if !(t0..t1).contains(&(ctx.Rip as usize)) && !(lo..hi).contains(&(ctx.Rip as usize)) {
                let sp = ctx.Rsp as usize as *const usize;
                for i in 0..1024 {
                    let v = sp.add(i).read_volatile();
                    if (t0 + 8..t1).contains(&v) && after_call(v) {
                        caller = v;
                        break;
                    }
                }
            }
            ctx.Rip as usize
        };
        let key = if (lo..hi).contains(&rip) {
            let i = starts.partition_point(|s| s.0 <= rip);
            if i == 0 { "?".to_string() } else { crate::image::named(starts[i - 1].1) }
        } else {
            let m = module_of(rip, &mut modules);
            match caller {
                0 => m,
                c if (lo..hi).contains(&c) => format!("{m} <- {}", crate::image::named(starts[starts.partition_point(|s| s.0 <= c) - 1].1)),
                c => format!("{m} <- {}", module_of(c, &mut modules)),
            }
        };
        *counts.entry(key).or_default() += 1;
        total += 1;
        if last.elapsed().as_secs() >= 2 {
            last = std::time::Instant::now();
            let mut v: Vec<_> = counts.drain().collect();
            v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            log!("sample: {total} samples");
            if let Some(path) = std::env::var_os("NSTRIKE_SAMPLE_RAW") {
                // every sample's name, for symbolising our own addresses against the link map
                let _ = std::fs::write(path, v.iter().map(|(k, n)| format!("{n} {k}
")).collect::<String>());
            }
            for (name, n) in v.iter().take(25) {
                log!("sample: {:5.1}% {name}", *n as f64 * 100.0 / total as f64);
            }
            total = 0;
        }
    }
}

/// The host module an address is in, with the offset in 4 KB pages for our own (".exe+page").
/// Looked up per 64 KB (`known`).
#[cfg(windows)]
fn module_of(a: usize, known: &mut HashMap<usize, Option<(usize, String)>>) -> String {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};
    let m = known.entry(a >> 16).or_insert_with(|| {
        let mut h = std::ptr::null_mut();
        if unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, a as *const u16, &mut h) } == 0 {
            return None;
        }
        let mut buf = [0u16; 260];
        let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), 260) } as usize;
        Some((h as usize, String::from_utf16_lossy(&buf[..n]).rsplit('\\').next().unwrap_or("?").to_lowercase()))
    });
    match m {
        Some((base, name)) if name.ends_with(".exe") || name == "ntdll.dll" => format!("{name}+{:#x}", a - *base),
        Some((_, name)) => name.clone(),
        None => format!("no module {:#x}", a & !0xffff),
    }
}

/// Our own code: the .text section's start and end.
#[cfg(windows)]
fn own_text() -> (usize, usize) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    static T: OnceLock<(usize, usize)> = OnceLock::new();
    *T.get_or_init(|| unsafe {
        let base = GetModuleHandleW(std::ptr::null()) as usize;
        let nt = base + (*((base + 0x3c) as *const u32)) as usize;
        let sections = *((nt + 6) as *const u16) as usize;
        let opt = *((nt + 20) as *const u16) as usize;
        let first = nt + 24 + opt;
        for i in 0..sections {
            let h = first + 40 * i;
            if std::slice::from_raw_parts(h as *const u8, 5) == b".text" {
                let (size, va) = (*((h + 8) as *const u32) as usize, *((h + 12) as *const u32) as usize);
                return (base + va, base + va + size);
            }
        }
        (0, 0)
    })
}

/// Is there a call instruction just before `a` (call rel32, call [rip+disp32], call reg/[reg])?
fn after_call(a: usize) -> bool {
    let b = |k: usize| unsafe { *((a - k) as *const u8) };
    b(5) == 0xe8 || (b(6) == 0xff && b(5) == 0x15) || (b(2) == 0xff && (b(1) & 0x38) == 0x10) || (b(3) == 0xff && (b(2) & 0x38) == 0x10)
}
