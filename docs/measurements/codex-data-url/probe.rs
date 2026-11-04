mod base;
mod variant;
use std::hint::black_box;
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "equal" {
        for len in (0..=1024).chain([65535,65536,65537,2097150,2097151,2097152]) {
            let bytes: Vec<_> = (0..len).map(|i| ((i * 73 + i / 7) & 255) as u8).collect();
            for mime in ["image/png", "image/jpeg", "image/gif", "image/webp", "IMAGE/PNG"] {
                assert_eq!(base::data_url(mime,&bytes),variant::data_url(mime,&bytes),"len={len}, mime={mime}");
            }
        }
        println!("5155 exact baseline/candidate data URL comparisons passed");
        return;
    }
    let len: usize = args[2].parse().unwrap();
    let bytes: Vec<_> = (0..len).map(|i| ((i * 73 + i / 7) & 255) as u8).collect();
    let operation = if args[1] == "base" { base::data_url } else { variant::data_url };
    drop(black_box(operation("image/png",black_box(&bytes)))); // unprofiled warm-up
    let profiler = dhat::Profiler::builder().testing().build();
    let output=black_box(operation(black_box("image/png"),black_box(&bytes)));
    let length=output.len();
    let stats=dhat::HeapStats::get();
    drop(output);
    drop(profiler);
    println!("{},{},{},{},{},{},{}",args[1],len,length,stats.total_blocks,stats.total_bytes,stats.max_blocks,stats.max_bytes);
}
