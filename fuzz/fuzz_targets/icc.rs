#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if data.len() > 4 * 1024 * 1024 {
        return;
    }
    let mut image = image::DynamicImage::new_rgba8(16, 16);
    let _ = bigimage_core::color::normalize_srgb(&mut image, data);
});
