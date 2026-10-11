#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;
fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    let tga = data.len() >= 18
        && [1, 2, 3, 9, 10, 11].contains(&data[2])
        && [8, 16, 24, 32].contains(&data[16]);
    let mut input =
        tempfile::Builder::new().suffix(if tga { ".tga" } else { ".png" }).tempfile().unwrap();
    input.write_all(data).unwrap();
    let dimensions = bigimage_core::backend::dimensions_in_process(input.path());
    // Keep full pixel work bounded under ASan; oversized headers remain parsed.
    if dimensions.is_ok_and(|(w, h)| u64::from(w) * u64::from(h) <= 1_000_000) {
        let _ = bigimage_core::backend::decode_in_process(input.path());
    }
    let _ = bigimage_core::metadata::read(input.path());
    let _ = bigimage_core::content::ensure_static_local(input.path());
});
