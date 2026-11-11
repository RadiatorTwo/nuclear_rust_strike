//! `NSTRIKE_SAMPLE=1`: where the game thread spends its time. A host thread stops the thread
//! that swaps Glide's buffers some 2000 times a second, reads its instruction pointer and
//! counts it for the recompiled function it is in (the nearest start below it; hooked
//! originals count for the function before them), or the host module. In system code the
//! thread's stack is copied while it stands and unwound afterwards (unwinding a stopped
//! thread can wait for a lock it holds), up to the first frame in our code: "ntdll <-
//! nvoglv64 <- nstrike.exe+off". Every 2 s the top entries are logged.
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
    use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, ReadProcessMemory, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_FULL_AMD64, UNW_FLAG_NHANDLER};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::{ResumeThread, SuspendThread};
    let Some(d) = crate::rt::DISPATCH.get() else { return };
    let mut starts: Vec<(usize, u32)> = d.table.iter().map(|&(g, p)| (p, g)).collect();
    starts.sort_unstable();
    let (lo, hi) = (starts.first().map_or(0, |s| s.0), starts.last().map_or(0, |s| s.0) + 0x10000);
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut modules = HashMap::new();
    let mut total = 0u32;
    let mut stack = vec![0u8; 64 << 10];
    let mut stack_len;
    let mut last = std::time::Instant::now();
    log!("sample: started");
    loop {
        std::thread::sleep(std::time::Duration::from_micros(500));
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.ContextFlags = CONTEXT_FULL_AMD64;
        let ours = |a: u64| (lo..hi).contains(&(a as usize)) || (own_text().0..own_text().1).contains(&(a as usize));
        unsafe {
            if SuspendThread(h) == u32::MAX {
                return;
            }
            let ok = GetThreadContext(h, &mut ctx);
            // in system code: its stack, copied while it stands (read without faulting)
            let mut got = 0;
            if ok != 0 && !ours(ctx.Rip) {
                while got < stack.len() {
                    let mut n = 0;
                    let p = (ctx.Rsp as usize + got) as *const std::ffi::c_void;
                    let chunk = (4096 - (p as usize & 4095)).min(stack.len() - got);
                    if ReadProcessMemory(GetCurrentProcess(), p, stack[got..].as_mut_ptr().cast(), chunk, &mut n) == 0 {
                        break;
                    }
                    got += n;
                }
            }
            ResumeThread(h);
            if ok == 0 {
                log!("sample: GetThreadContext failed: {}", std::io::Error::last_os_error());
                return;
            }
            stack_len = got;
        }
        let rip = ctx.Rip as usize;
        let name = |a: usize, modules: &mut HashMap<usize, Option<(usize, String)>>| {
            if (lo..hi).contains(&a) {
                let i = starts.partition_point(|s| s.0 <= a);
                match i.checked_sub(1).map(|i| starts[i]) {
                    None => "?".to_string(),
                    // the function NSTRIKE_DUMP shows: by place in its machine code (the dump's
                    // offsets)
                    Some((p, g)) if crate::jit::dump_of(g) => format!("{}+{:x}", crate::image::named(g), a - p),
                    Some((_, g)) => crate::image::named(g),
                }
            } else {
                module_of(a, modules)
            }
        };
        let mut key = name(rip, &mut modules);
        if !ours(ctx.Rip) && stack_len > 0 {
            // unwound on the copy, up to the first frame in our code or the game's
            let base = stack.as_ptr() as u64;
            let mut c = ctx;
            c.Rsp = base;
            // (a module's frames one after another named once: the driver's run deep)
            let mut last = key.clone();
            for _ in 0..40 {
                let mut image = 0u64;
                let f = unsafe { RtlLookupFunctionEntry(c.Rip, &mut image, std::ptr::null_mut()) };
                if !(base..base + stack_len as u64 - 8).contains(&c.Rsp) {
                    break;
                }
                if f.is_null() {
                    c.Rip = unsafe { *(c.Rsp as *const u64) };
                    c.Rsp += 8;
                } else {
                    let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
                    unsafe { RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, c.Rip, f, &mut c, &mut data, &mut frame, std::ptr::null_mut()) };
                }
                if c.Rip == 0 {
                    break;
                }
                let n = name(c.Rip as usize, &mut modules);
                if n != last || ours(c.Rip) {
                    key = format!("{key} <- {n}");
                    last = n;
                }
                if ours(c.Rip) {
                    break;
                }
            }
        }
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
