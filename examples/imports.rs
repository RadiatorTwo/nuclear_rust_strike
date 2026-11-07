//! The EXE's imports: IAT slot, DLL, name.  imports <NUKEPC.EXE>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let img = nstrike::image::Image::load(std::path::Path::new(&a[1])).unwrap();
    for i in img.imports() {
        println!("{:08x} {} {}", i.iat, i.dll, i.name);
    }
}
