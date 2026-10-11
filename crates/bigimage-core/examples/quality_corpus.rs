// SPDX-License-Identifier: GPL-3.0-or-later
//! Generate synthetic codec/ICC seeds; never reads personal images.
use std::io::Write;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root =
        std::path::PathBuf::from(std::env::args_os().nth(1).ok_or("provide output directory")?);
    std::fs::create_dir_all(root.join("decode"))?;
    std::fs::create_dir_all(root.join("icc"))?;
    let input = root.join("decode/source.png");
    image::RgbImage::from_fn(32, 24, |x, y| image::Rgb([(x * 7) as u8, (y * 11) as u8, 150]))
        .save(&input)?;
    for format in bigimage_core::Format::all() {
        bigimage_core::convert_file_to(
            &input,
            None,
            *format,
            &bigimage_core::EncodeOptions::default(),
            bigimage_core::OverwritePolicy::Replace,
        )?;
    }
    for (name, profile) in [
        ("srgb", moxcms::ColorProfile::new_srgb()),
        ("display-p3", moxcms::ColorProfile::new_display_p3()),
    ] {
        std::fs::File::create(root.join("icc").join(name))?.write_all(&profile.encode()?)?;
    }
    Ok(())
}
