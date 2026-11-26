// SPDX-License-Identifier: GPL-3.0-or-later
//! BigIris — modern image viewer and converter for Linux.
//!
//! Single binary, three entry modes:
//! 1. Viewer: `bigiris [FILE...]`
//! 2. Dialog: `bigiris --dialog=<name> [FILE...]` (used by file-manager service menus)
//! 3. Headless CLI: `bigiris <subcommand> ...` (used by CI/CD and quick service-menu actions)

// `deny`, not `forbid`: two audited `unsafe` sites are allowed, the
// `pre_exec` hook in `codec::close_inherited_descriptors` and the
// `setlocale` call at the start of `main`.
#![deny(unsafe_code)]

// Faster allocator for the multi-thread batch path. ~5-15% wall-clock
// win on a 100-file convert batch on a 16-thread CPU per the perf
// review (`mimalloc` ships with low fragmentation tuned for short-
// lived multi-thread workloads, which is exactly what every Prisma
// dialog produces). Statically linked; no runtime dep.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod codec;
#[cfg(feature = "gui")]
mod gui;

use std::ops::ControlFlow::Continue;
use std::path::{Path, PathBuf};

/// One exit-status convention for every headless command.
const EXIT_PARTIAL: i32 = 2;

use bigimage_core::{
    AdjustOps, AnimateOptions, ConvertOutcome, CropRect, EncodeOptions, ExtentOptions, ExtentSize,
    Filter, FlipAxis, Format, Gravity, LoopMode, OverwritePolicy, QualityTarget, ResizeMode,
    Rotation,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gettextrs::gettext;

#[derive(Parser, Debug)]
#[command(
    name = "bigiris",
    version,
    about = gettext("Fast image viewer and converter for Linux"),
    long_about = None,
    after_help = gettext("Exit codes:\n  0  success (includes files skipped because they already exist)\n  1  invalid usage or fatal error before processing (nothing was written)\n  2  partial failure: one or more files could not be processed"),
)]
struct Cli {
    #[arg(
        long,
        value_name = "NAME",
        help = gettext("Open only one specific modal dialog (used by service menus)")
    )]
    dialog: Option<Dialog>,

    #[arg(long, help = gettext("Run a smoke test and exit (used in CI and after installation)"))]
    self_test: bool,

    #[arg(
        long,
        global = true,
        help = gettext("Report failures in a desktop notification (used by context menus, which do not show the terminal)")
    )]
    notify: bool,

    #[arg(
        value_name = "FILE",
        help = gettext("Files to open in the viewer (when there is no subcommand)")
    )]
    files: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum Dialog {
    Convert,
    Resize,
    Rotate,
    Flip,
    Crop,
    Extent,
    Adjust,
    Metadata,
    Compare,
    Animate,
    RemoveBg,
    Upscale,
    Batch,
}

#[derive(ValueEnum, Debug, Clone, Copy, Default)]
#[value(rename_all = "kebab-case")]
enum OverwriteArg {
    #[default]
    #[value(help = gettext("Does not overwrite the existing file (default)"))]
    Skip,
    #[value(help = gettext("Overwrites the existing file"))]
    Replace,
    #[value(help = gettext("Generates an incremented name (foo_1.png, foo_2.png, ...)"))]
    Increment,
}

impl From<OverwriteArg> for OverwritePolicy {
    fn from(v: OverwriteArg) -> Self {
        match v {
            OverwriteArg::Skip => OverwritePolicy::Skip,
            OverwriteArg::Replace => OverwritePolicy::Replace,
            OverwriteArg::Increment => OverwritePolicy::Increment,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, Default)]
#[value(rename_all = "kebab-case")]
enum FilterArg {
    Nearest,
    Bilinear,
    CatmullRom,
    Mitchell,
    #[default]
    Lanczos3,
}

impl From<FilterArg> for Filter {
    fn from(v: FilterArg) -> Self {
        match v {
            FilterArg::Nearest => Filter::Nearest,
            FilterArg::Bilinear => Filter::Bilinear,
            FilterArg::CatmullRom => Filter::CatmullRom,
            FilterArg::Mitchell => Filter::Mitchell,
            FilterArg::Lanczos3 => Filter::Lanczos3,
        }
    }
}

#[derive(Args, Debug, Clone)]
#[group(required = true, multiple = false)]
struct ResizeModeArgs {
    #[arg(
        long = "max-edge",
        help = gettext("Maximum edge length in pixels (keeps the aspect ratio; no-op if the image is already smaller)")
    )]
    max_edge: Option<u32>,
    #[arg(long, help = gettext("Percentage scale (e.g. 50 for half; 200 for double)"))]
    percent: Option<f32>,
    #[arg(long, value_name = "WxH", help = gettext("Exact WxH size (ignores the aspect ratio)"))]
    exact: Option<WxH>,
    #[arg(
        long,
        value_name = "WxH",
        help = gettext("Fits inside the WxH box, keeping the aspect ratio")
    )]
    fit: Option<WxH>,
}

impl ResizeModeArgs {
    fn into_mode(self) -> ResizeMode {
        match self {
            Self { max_edge: Some(e), .. } => ResizeMode::MaxEdge(e),
            Self { percent: Some(p), .. } => ResizeMode::Percent(p),
            Self { exact: Some(d), .. } => ResizeMode::Exact { width: d.width, height: d.height },
            Self { fit: Some(d), .. } => ResizeMode::Fit { width: d.width, height: d.height },
            // clap enforces required=true + multiple=false on the ArgGroup, so
            // we're guaranteed exactly one of the above is Some.
            _ => unreachable!("the clap ArgGroup requires exactly one mode"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WxH {
    width: u32,
    height: u32,
}

impl std::str::FromStr for WxH {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| {
            fill(
                &gettext("expected the WxH format (e.g. 1920x1080), got: {value}"),
                &[("value", &format!("{s:?}"))],
            )
        })?;
        let width: u32 =
            w.parse().map_err(|e| with_error(&gettext("invalid width: {error}"), e))?;
        let height: u32 =
            h.parse().map_err(|e| with_error(&gettext("invalid height: {error}"), e))?;
        Ok(WxH { width, height })
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "verbatim")]
enum RotationArg {
    #[value(name = "90")]
    Deg90,
    #[value(name = "180")]
    Deg180,
    #[value(name = "270")]
    Deg270,
}

impl From<RotationArg> for Rotation {
    fn from(v: RotationArg) -> Self {
        match v {
            RotationArg::Deg90 => Rotation::Deg90,
            RotationArg::Deg180 => Rotation::Deg180,
            RotationArg::Deg270 => Rotation::Deg270,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "kebab-case")]
enum FlipAxisArg {
    Horizontal,
    Vertical,
}

impl From<FlipAxisArg> for FlipAxis {
    fn from(v: FlipAxisArg) -> Self {
        match v {
            FlipAxisArg::Horizontal => FlipAxis::Horizontal,
            FlipAxisArg::Vertical => FlipAxis::Vertical,
        }
    }
}

/// ImageMagick-style crop geometry: `WxH+X+Y`.
#[derive(Debug, Clone, Copy)]
struct CropGeom {
    rect: CropRect,
}

impl std::str::FromStr for CropGeom {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split on first '+': "WxH" + "X+Y"
        let quoted = format!("{s:?}");
        let malformed = |template: String| fill(&template, &[("value", &quoted)]);
        let (wh, rest) = s
            .split_once('+')
            .ok_or_else(|| malformed(gettext("expected WxH+X+Y, got: {value}")))?;
        let (x, y) = rest
            .split_once('+')
            .ok_or_else(|| malformed(gettext("missing the Y offset in {value}")))?;
        let (w, h) = wh
            .split_once(['x', 'X'])
            .ok_or_else(|| malformed(gettext("missing width/height in {value}")))?;
        let width: u32 =
            w.parse().map_err(|e| with_error(&gettext("invalid width: {error}"), e))?;
        let height: u32 =
            h.parse().map_err(|e| with_error(&gettext("invalid height: {error}"), e))?;
        let x: u32 = x.parse().map_err(|e| with_error(&gettext("invalid X offset: {error}"), e))?;
        let y: u32 = y.parse().map_err(|e| with_error(&gettext("invalid Y offset: {error}"), e))?;
        Ok(CropGeom { rect: CropRect::new(x, y, width, height) })
    }
}

/// Aspect ratio in the form `W:H` (e.g. `1:1`, `9:16`, `4:5`). Whitespace
/// around the separator is tolerated so users can quote `"9 : 16"` without
/// the parse failing.
#[derive(Debug, Clone, Copy)]
struct Aspect {
    w: u32,
    h: u32,
}

impl std::str::FromStr for Aspect {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let quoted = format!("{s:?}");
        let (a, b) = trimmed.split_once(':').ok_or_else(|| {
            fill(&gettext("expected W:H (e.g. 1:1, 9:16), got: {value}"), &[("value", &quoted)])
        })?;
        let w: u32 =
            a.trim().parse().map_err(|e| with_error(&gettext("invalid W ratio: {error}"), e))?;
        let h: u32 =
            b.trim().parse().map_err(|e| with_error(&gettext("invalid H ratio: {error}"), e))?;
        if w == 0 || h == 0 {
            return Err(fill(
                &gettext("a ratio with a zero component makes no sense: {value}"),
                &[("value", &quoted)],
            ));
        }
        Ok(Aspect { w, h })
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, Default)]
#[value(rename_all = "kebab-case")]
enum GravityArg {
    NorthWest,
    North,
    NorthEast,
    West,
    #[default]
    Center,
    East,
    SouthWest,
    South,
    SouthEast,
}

impl From<GravityArg> for Gravity {
    fn from(v: GravityArg) -> Self {
        match v {
            GravityArg::NorthWest => Gravity::NorthWest,
            GravityArg::North => Gravity::North,
            GravityArg::NorthEast => Gravity::NorthEast,
            GravityArg::West => Gravity::West,
            GravityArg::Center => Gravity::Center,
            GravityArg::East => Gravity::East,
            GravityArg::SouthWest => Gravity::SouthWest,
            GravityArg::South => Gravity::South,
            GravityArg::SouthEast => Gravity::SouthEast,
        }
    }
}

/// Required exactly-one selector between explicit size and aspect ratio.
#[derive(Args, Debug, Clone)]
#[group(required = true, multiple = false)]
struct ExtentSizeArgs {
    #[arg(long, value_name = "WxH", help = gettext("Exact canvas size in pixels (WxH)"))]
    size: Option<WxH>,
    #[arg(
        long,
        value_name = "W:H",
        help = gettext("Target ratio (W:H). The canvas is extended as little as needed to fit the image in that ratio")
    )]
    aspect: Option<Aspect>,
}

impl ExtentSizeArgs {
    fn into_size(self) -> ExtentSize {
        match self {
            Self { size: Some(d), .. } => ExtentSize::Exact { width: d.width, height: d.height },
            Self { aspect: Some(a), .. } => ExtentSize::Aspect { w: a.w, h: a.h },
            _ => unreachable!("the clap ArgGroup requires exactly one mode"),
        }
    }
}

/// Encoder settings shared by the commands that write a re-encoded image.
#[derive(Args, Debug, Clone)]
struct EncodeArgs {
    #[arg(
        long,
        value_name = "MODE",
        default_value = "balanced",
        value_parser = parse_target,
        help = gettext("Visual quality to aim for: maximum, balanced or smallest (also max, small). Each format looks for the smallest file that keeps this quality (SSIMULACRA2 85 / 80 / 70)")
    )]
    target: QualityTarget,
    #[arg(
        long,
        value_parser = clap::value_parser!(u8).range(1..=100),
        help = gettext("Fixed quality 1..100 for JPEG/WebP/AVIF, without searching (ignores --target). In WebP, 100 writes losslessly")
    )]
    quality: Option<u8>,
    #[arg(long, help = gettext("Accepted for compatibility: JPEG is always progressive"))]
    progressive: bool,
    #[arg(long, help = gettext("Slower compression and smaller files, with no extra loss: PNG and AVIF"))]
    optimize: bool,
}

impl EncodeArgs {
    fn options(self) -> EncodeOptions {
        let Self { target, quality, progressive, optimize } = self;
        EncodeOptions { quality, progressive, optimize, target, ..EncodeOptions::default() }
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = gettext("Convert files (headless)"))]
    Convert {
        #[arg(
            long,
            help = gettext("Target format: png, jpg, webp, avif, tiff, bmp, gif, ico, pnm/ppm/pgm/pbm, tga, qoi, hdr, exr")
        )]
        to: String,
        #[command(flatten)]
        encode: EncodeArgs,
        #[arg(
            long = "overwrite",
            value_enum,
            default_value_t = OverwriteArg::Skip,
            help = gettext("What to do when the output file already exists")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true, help = gettext("Input files"))]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Resize files (headless). Exactly one mode among --max-edge / --percent / --exact / --fit is required"))]
    Resize {
        #[command(flatten)]
        mode: ResizeModeArgs,
        #[arg(
            long,
            value_enum,
            default_value_t = FilterArg::Lanczos3,
            help = gettext("Interpolation kernel (default: lanczos3)")
        )]
        filter: FilterArg,
        #[arg(long, help = gettext("Target format (optional; keeps the original format if omitted)"))]
        to: Option<String>,
        #[command(flatten)]
        encode: EncodeArgs,
        #[arg(
            long = "overwrite",
            value_enum,
            default_value_t = OverwriteArg::Skip,
            help = gettext("What to do when the output file already exists")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Rotate files (headless). Right-angle rotations through a re-encode pipeline; `--auto` reads the EXIF Orientation tag and applies the right rotation"))]
    Rotate {
        #[arg(
            long,
            value_enum,
            conflicts_with = "auto",
            help = gettext("Rotation in degrees: 90 | 180 | 270")
        )]
        degrees: Option<RotationArg>,
        #[arg(
            long,
            conflicts_with = "degrees",
            help = gettext("Auto-orient from EXIF (Orientation tag)")
        )]
        auto: bool,
        #[arg(long, help = gettext("Target format (optional; keeps the original format if omitted)"))]
        to: Option<String>,
        #[arg(
            long = "overwrite",
            value_enum,
            default_value_t = OverwriteArg::Skip,
            help = gettext("What to do when the output file already exists")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Mirror files horizontally or vertically (headless)"))]
    Flip {
        #[arg(long, value_enum, help = gettext("Axis: horizontal | vertical"))]
        axis: FlipAxisArg,
        #[arg(long, help = gettext("Target format (optional)"))]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Crop a rectangle from images (headless). Use --rect WxH+X+Y, ImageMagick style"))]
    Crop {
        #[arg(
            long,
            help = gettext("Crop geometry in the WxH+X+Y format (e.g. 800x600+100+50)")
        )]
        rect: CropGeom,
        #[arg(long, help = gettext("Target format (optional)"))]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Extend the canvas around the image (letterbox). Exactly one mode among --size and --aspect is required. The canvas is filled with --bg and the original image is placed by --gravity"))]
    Extent {
        #[command(flatten)]
        size: ExtentSizeArgs,
        #[arg(
            long,
            default_value = "#000000",
            help = gettext("Background color in hexadecimal (#RRGGBB or #RRGGBBAA)")
        )]
        bg: String,
        #[arg(
            long,
            value_enum,
            default_value_t = GravityArg::Center,
            help = gettext("Position of the original image on the extended canvas")
        )]
        gravity: GravityArg,
        #[arg(long, help = gettext("Target format (optional)"))]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Adjust color/tone (brightness, contrast, saturation, gamma)"))]
    Adjust {
        #[arg(
            long,
            default_value_t = 0,
            allow_negative_numbers = true,
            help = gettext("Brightness -100..100 (0 = no change)")
        )]
        brightness: i32,
        #[arg(
            long,
            default_value_t = 0.0,
            allow_negative_numbers = true,
            help = gettext("Contrast -100..100 (0 = no change)")
        )]
        contrast: f32,
        #[arg(
            long,
            default_value_t = 0.0,
            allow_negative_numbers = true,
            help = gettext("Saturation -100..100 (-100 = gray, 0 = no change)")
        )]
        saturation: f32,
        #[arg(
            long,
            default_value_t = 1.0,
            help = gettext("Gamma 0.1..10.0 (< 1 brightens midtones, > 1 darkens)")
        )]
        gamma: f32,
        #[arg(long, help = gettext("Target format (optional)"))]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Remove the background with local AI (BiRefNet-lite, birefnet-native package). Writes `<name>_nobg.png` (PNG with alpha) next to each image"))]
    RemoveBg {
        #[arg(
            long,
            value_enum,
            default_value_t = ModelArg::BirefnetLite,
            help = gettext("Segmentation model")
        )]
        model: ModelArg,
        #[arg(
            long = "overwrite",
            value_enum,
            default_value_t = OverwriteArg::Skip,
            help = gettext("What to do when `<name>_nobg.png` already exists")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Increase resolution by 2, 3 or 4. With the realesrgan-native package it uses AI (Real-ESRGAN); without it, Lanczos3. Writes `<name>_<pct>pct.<ext>`"))]
    Upscale {
        #[arg(long, default_value_t = 2, help = gettext("Integer factor (2, 3 or 4)"))]
        factor: u8,
        #[arg(
            long,
            value_enum,
            default_value_t = UpscaleEngineArg::Auto,
            help = gettext("Engine: auto (Real-ESRGAN if installed, otherwise Lanczos3), realesrgan (fails if missing) or lanczos")
        )]
        engine: UpscaleEngineArg,
        #[arg(
            long,
            default_value_t,
            value_enum,
            help = gettext("Overwrite policy for the output file")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Remove noise and compression artifacts with local AI (SCUNet, scunet-native package). Writes `<name>_denoised.<ext>`"))]
    Denoise {
        #[arg(
            long,
            default_value_t,
            value_enum,
            help = gettext("Overwrite policy for the output file")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Erase objects: fills the area marked in the mask with local AI (LaMa, lama-native package). Writes `<name>_inpaint.<ext>`"))]
    Inpaint {
        #[arg(
            long,
            value_name = "MASK",
            help = gettext("Mask the same size as the image: white = fill, black = keep (transparent areas are kept)")
        )]
        mask: PathBuf,
        #[arg(
            long,
            default_value_t,
            value_enum,
            help = gettext("Overwrite policy for the output file")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Create an animated GIF from a sequence of images (headless)"))]
    Animate {
        #[arg(long, short = 'o', help = gettext("Output file (.gif)"))]
        output: PathBuf,
        #[arg(
            long,
            default_value_t = 100,
            help = gettext("Delay between frames in milliseconds (100 ≈ 10 fps)")
        )]
        delay: u32,
        #[arg(
            long,
            default_value_t = 0,
            help = gettext("Number of repetitions: 0 = infinite (default), 1+ = fixed")
        )]
        loop_count: u16,
        #[arg(
            long,
            default_value_t = 10,
            help = gettext("Encoder speed 1..30 (higher = faster, worse palette)")
        )]
        speed: i32,
        #[arg(
            long,
            default_value_t,
            value_enum,
            help = gettext("Overwrite policy for the output file")
        )]
        overwrite: OverwriteArg,
        #[arg(value_name = "FRAME", required = true, help = gettext("Frames in order (at least 1)"))]
        frames: Vec<PathBuf>,
    },
    #[command(about = gettext("Process files or folders in parallel from a TOML profile"))]
    Batch {
        #[arg(long, help = gettext("Profile file (.toml)"))]
        profile: PathBuf,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    #[command(about = gettext("Install file manager integrations"))]
    InstallIntegrations {
        #[arg(long, conflicts_with = "system", help = gettext("Install in ~/.local/share (default)"))]
        user: bool,
        #[arg(
            long,
            help = gettext("Install in /usr/share (requires root; usually done by the package)")
        )]
        system: bool,
        #[arg(
            long,
            value_name = "PATH",
            requires = "system",
            conflicts_with = "user",
            help = gettext("DESTDIR prefix for packaging (e.g. $pkgdir in the PKGBUILD). Only makes sense together with --system")
        )]
        destdir: Option<PathBuf>,
    },
    #[command(about = gettext("Remove previously installed integrations"))]
    UninstallIntegrations,
    #[command(about = gettext("Convert documents (ODT, DOCX, TXT, MD, RTF, XLSX, PPTX, etc.) to PDF with headless LibreOffice. The output is written next to the source file"))]
    ToPdf {
        #[arg(
            long = "overwrite",
            value_enum,
            default_value_t = OverwriteArg::Skip,
            help = gettext("What to do when `<name>.pdf` already exists")
        )]
        overwrite: OverwriteArg,
        #[arg(
            long,
            default_value_t = 120,
            value_parser = clap::value_parser!(u64).range(1..=3600),
            help = gettext("Maximum time per document, in seconds")
        )]
        timeout: u64,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
}

/// Background-removal models this build can actually run.
#[derive(ValueEnum, Debug, Clone, Copy, Default)]
#[value(rename_all = "kebab-case")]
enum ModelArg {
    #[default]
    #[value(help = gettext("BiRefNet-lite (the only model available)"))]
    BirefnetLite,
}

#[derive(ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
enum UpscaleEngineArg {
    #[default]
    #[value(help = gettext("Real-ESRGAN when the realesrgan-native package is installed, otherwise Lanczos3"))]
    Auto,
    #[value(
        alias = "ai",
        alias = "real-esrgan",
        help = gettext("Real-ESRGAN AI (requires realesrgan-native)")
    )]
    Realesrgan,
    #[value(alias = "lanczos3", alias = "cpu", help = gettext("Lanczos3 resampling (no AI)"))]
    Lanczos,
}

fn main() -> color_eyre::Result<()> {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--codec-worker") {
        return codec::worker();
    }
    apply_user_locale();
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bigiris=info,bigimage_core=info".into()),
        )
        .init();

    init_i18n();

    // clap exits with 2 on usage errors; 2 means partial failure here.
    let cli = Cli::try_parse().unwrap_or_else(|error| {
        let _ = error.print();
        std::process::exit(i32::from(error.use_stderr()));
    });

    codec::configure();
    NOTIFY.store(cli.notify, std::sync::atomic::Ordering::Relaxed);

    if cli.self_test {
        return self_test();
    }

    let result = match cli.command {
        Some(Command::Convert { to, encode, overwrite, files }) => {
            run_convert(&to, encode.options(), overwrite.into(), &files)
        }
        Some(Command::Resize { mode, filter, to, encode, overwrite, files }) => run_resize(
            mode.into_mode(),
            filter.into(),
            to.as_deref(),
            encode.options(),
            overwrite.into(),
            &files,
        ),
        Some(Command::Rotate { degrees, auto, to, overwrite, files }) => {
            let target = parse_optional_target(to.as_deref())?;
            let policy: OverwritePolicy = overwrite.into();
            if auto {
                tracing::info!(?target, ?policy, count = files.len(), "rotate auto (EXIF)");
                run_per_file(&files, "rotate", Naming::image(target, policy), |f| {
                    bigimage_core::rotate_file_auto(f, target, policy)
                })
            } else {
                let rotation: Rotation = degrees
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!(gettext(
                            "rotate: use --degrees 90|180|270 or --auto"
                        ))
                    })?
                    .into();
                tracing::info!(?rotation, ?target, ?policy, count = files.len(), "rotate");
                run_per_file(&files, "rotate", Naming::image(target, policy), |f| {
                    bigimage_core::rotate_file(f, rotation, target, policy)
                })
            }
        }
        Some(Command::Flip { axis, to, overwrite, files }) => {
            run_flip(axis.into(), to.as_deref(), overwrite.into(), &files)
        }
        Some(Command::Crop { rect, to, overwrite, files }) => {
            run_crop(rect.rect, to.as_deref(), overwrite.into(), &files)
        }
        Some(Command::Extent { size, bg, gravity, to, overwrite, files }) => run_extent(
            size.into_size(),
            &bg,
            gravity.into(),
            to.as_deref(),
            overwrite.into(),
            &files,
        ),
        Some(Command::Adjust { brightness, contrast, saturation, gamma, to, overwrite, files }) => {
            run_adjust(
                AdjustOps { brightness, contrast, saturation, gamma },
                to.as_deref(),
                overwrite.into(),
                &files,
            )
        }
        Some(Command::RemoveBg { model, overwrite, files }) => {
            run_remove_bg(model, overwrite.into(), &files)
        }
        Some(Command::Upscale { factor, engine, overwrite, files }) => {
            run_upscale(factor, engine, overwrite.into(), &files)
        }
        Some(Command::Denoise { overwrite, files }) => run_denoise(overwrite.into(), &files),
        Some(Command::Inpaint { mask, overwrite, files }) => {
            run_inpaint(&mask, overwrite.into(), &files)
        }
        Some(Command::Animate { output, delay, loop_count, speed, overwrite, frames }) => {
            run_animate(&output, delay, loop_count, speed, overwrite.into(), &frames)
        }
        Some(Command::Batch { profile, files }) => run_batch(&profile, &files),
        Some(Command::InstallIntegrations { user, system, destdir }) => {
            run_install(user, system, destdir)
        }
        Some(Command::UninstallIntegrations) => run_uninstall(),
        Some(Command::ToPdf { overwrite, timeout, files }) => {
            run_to_pdf(&files, overwrite.into(), std::time::Duration::from_secs(timeout))
        }
        None => dispatch_gui(cli.dialog, &cli.files),
    };
    if let Err(error) = &result {
        notify_failure(&error.to_string());
    }
    result
}

/// Set by `--notify`: a file-manager menu started this run.
static NOTIFY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Show a failure on the desktop when the run came from a file-manager menu,
/// where nobody reads stderr. The details stay on stderr as always.
fn notify_failure(message: &str) {
    if !NOTIFY.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let shown = codec::close_inherited_descriptors(&mut std::process::Command::new("notify-send"))
        .args(["--app-name=BigIris", "--icon=com.biglinux.Iris", "--"])
        .arg(gettext("BigIris: the operation failed"))
        .arg(message)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(error) = shown {
        tracing::warn!(%error, "notify-send unavailable; the failure went to stderr only");
    }
}

/// End a run in which some files failed (exit status 2).
fn exit_partial() -> ! {
    notify_failure(&gettext("Some files were not processed; see the details in the terminal."));
    std::process::exit(EXIT_PARTIAL)
}

/// Bind the `bigiris` textdomain to `/usr/share/locale` (the system path
/// where the PKGBUILD installs compiled `.mo` catalogs). Each call returns
/// `Result`; any failure means gettext
/// silently returns source strings — acceptable graceful degrade for a
/// dev `cargo run` where no catalogs are installed yet.
fn init_i18n() {
    let locale_dir =
        std::env::var("BIGIRIS_LOCALE_DIR").unwrap_or_else(|_| "/usr/share/locale".to_owned());
    let _ = gettextrs::bindtextdomain("bigiris", locale_dir);
    let _ = gettextrs::bind_textdomain_codeset("bigiris", "UTF-8");
    let _ = gettextrs::textdomain("bigiris");
}

/// Take the user's locale from the environment, so gettext answers in it.
#[allow(unsafe_code)]
fn apply_user_locale() {
    // SAFETY: setlocale is not thread-safe; `main` calls this before it, or
    // anything it calls, starts a thread.
    unsafe { gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "") };
}

/// Substitute `{name}` placeholders of a translated template in one pass,
/// so a value that itself contains `{…}` (a file name) is never expanded.
pub(crate) fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let key = &after[..close];
            values.iter().find(|(name, _)| *name == key).map(|(_, value)| (close, *value))
        });
        match value {
            Some((close, value)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `template` with its `{error}` placeholder replaced by `error`.
fn with_error(template: &str, error: impl std::fmt::Display) -> String {
    fill(template, &[("error", &error.to_string())])
}

fn self_test() -> color_eyre::Result<()> {
    codec::self_test()?;
    println!(
        "bigiris self-test OK [sandbox + decode]  (bigiris={}+{}  core={}  ai={} [engines={}]  integrations={})",
        env!("CARGO_PKG_VERSION"),
        env!("BIGIRIS_BUILD_ID"),
        bigimage_core::version(),
        bigimage_ai::version(),
        bigimage_ai::engines_compiled(),
        bigimage_integrations::version(),
    );
    Ok(())
}

/// `--target` and the profile `target`.
fn parse_target(value: &str) -> Result<QualityTarget, String> {
    QualityTarget::parse(value).ok_or_else(|| {
        fill(
            &gettext("invalid quality target: {value}; use maximum, balanced or smallest"),
            &[("value", &format!("{value:?}"))],
        )
    })
}

fn run_convert(
    to: &str,
    opts: EncodeOptions,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = Format::parse(to).map_err(|e| {
        color_eyre::eyre::eyre!(with_error(&gettext("invalid target format: {error}"), e))
    })?;
    tracing::info!(?target, ?opts, ?policy, count = files.len(), "convert");
    run_per_file(files, "convert", Naming::image(Some(target), policy), |f| {
        let report = bigimage_core::convert_file_report(f, None, target, &opts, policy)?;
        if report.kept_original {
            // allow-noisy-log: `info` lines are part of the command's report.
            println!(
                "info {}: {}",
                f.display(),
                gettext("the original was already smaller; copied without recompression")
            );
        }
        Ok(report.outcome)
    })
}

fn run_resize(
    mode: ResizeMode,
    filter: Filter,
    to: Option<&str>,
    opts: EncodeOptions,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?mode, ?filter, ?target, ?opts, ?policy, count = files.len(), "resize");
    run_per_file(files, "resize", Naming::image(target, policy), |f| {
        bigimage_core::resize_file(f, mode, filter, target, &opts, policy)
    })
}

fn run_flip(
    axis: FlipAxis,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?axis, ?target, ?policy, count = files.len(), "flip");
    run_per_file(files, "flip", Naming::image(target, policy), |f| {
        bigimage_core::flip_file(f, axis, target, policy)
    })
}

fn run_crop(
    rect: CropRect,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?rect, ?target, ?policy, count = files.len(), "crop");
    run_per_file(files, "crop", Naming::image(target, policy), |f| {
        bigimage_core::crop_file(f, rect, target, policy)
    })
}

fn run_extent(
    size: ExtentSize,
    bg: &str,
    gravity: Gravity,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    let background = bigimage_core::parse_hex_color(bg)
        .map_err(|e| color_eyre::eyre::eyre!(with_error(&gettext("invalid --bg: {error}"), e)))?;
    let opts = ExtentOptions { size, background, gravity };
    tracing::info!(?opts, ?target, ?policy, count = files.len(), "extent");
    let naming = Naming {
        per_image_suffix: matches!(size, ExtentSize::Aspect { .. }),
        ..Naming::image(target, policy)
    };
    run_per_file(files, "extent", naming, |f| bigimage_core::extent_file(f, opts, target, policy))
}

fn run_adjust(
    ops: AdjustOps,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?ops, ?target, ?policy, count = files.len(), "adjust");
    run_per_file(files, "adjust", Naming::image(target, policy), |f| {
        bigimage_core::adjust_file(f, ops, target, policy)
    })
}

fn parse_optional_target(to: Option<&str>) -> color_eyre::Result<Option<Format>> {
    match to {
        Some(s) => Ok(Some(Format::parse(s).map_err(|e| {
            color_eyre::eyre::eyre!(with_error(&gettext("invalid target format: {error}"), e))
        })?)),
        None => Ok(None),
    }
}

/// How a command names outputs: `<dir>/<stem><suffix>.<ext>`. The suffix is
/// usually fixed per invocation, so inputs sharing directory, stem and
/// extension resolve to the same file.
#[derive(Clone, Copy)]
struct Naming<'a> {
    output_dir: Option<&'a Path>,
    /// `None` keeps each input's own format.
    extension: Option<&'static str>,
    policy: OverwritePolicy,
    /// The suffix depends on each image (`extent --aspect` names the output
    /// after the resulting canvas), so a shared stem is only a possible
    /// collision: it is not refused up front, and [`run_per_file`] reports
    /// an input that really replaced an earlier output.
    per_image_suffix: bool,
}

impl Naming<'_> {
    fn image(target: Option<Format>, policy: OverwritePolicy) -> Naming<'static> {
        Naming {
            output_dir: None,
            extension: target.map(|format| format.extension()),
            policy,
            per_image_suffix: false,
        }
    }
}

/// Group inputs by destination, in input order, before any work starts.
/// Repeated spellings of one file (`a.png`, `./a.png`) are processed once.
/// Distinct inputs that collide are refused unless the policy is
/// `increment`; callers then process each group in order, so the numbered
/// names are deterministic. Under `increment`, an input named like another
/// input's numbered output (`x_1` next to `x`) joins that group as well.
///
/// PNM is planned conservatively under one key (`ppm`): the core only picks
/// `.pgm` or `.ppm` from the decoded pixels, so a gray and a colour input
/// sharing a stem are refused although they would not collide. Telling
/// them apart here would mean decoding untrusted headers in this process
/// (or one extra sandboxed run per input) before any work starts; the cost
/// of the false refusal is a rename. Reports always print the path the core
/// actually wrote ([`ConvertOutcome`]), never this planned one.
fn plan_outputs(files: &[PathBuf], naming: Naming) -> color_eyre::Result<Vec<Vec<PathBuf>>> {
    use std::collections::{HashMap, HashSet};
    let mut seen = HashSet::new();
    let mut groups: Vec<Vec<PathBuf>> = Vec::new();
    let mut index = HashMap::new();
    let mut planned = Vec::new();
    for file in files {
        let identity = std::fs::canonicalize(file)
            .or_else(|_| std::path::absolute(file))
            .unwrap_or_else(|_| file.clone());
        if !seen.insert(identity) {
            continue;
        }
        let dir = naming
            .output_dir
            .or_else(|| file.parent())
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        // Without a target the core keeps the format found in the content,
        // so `a.jpg` holding a PNG writes `a_<suffix>.png`; plan by the same.
        let extension = naming.extension.map(str::to_owned).unwrap_or_else(|| {
            bigimage_core::sniff_format(file).map_or_else(
                || {
                    let own = file.extension().and_then(|e| e.to_str()).unwrap_or_default();
                    own.to_ascii_lowercase()
                },
                |format| format.extension().to_owned(),
            )
        });
        planned.push(((dir, file.file_stem().unwrap_or_default().to_os_string(), extension), file));
    }
    let keys: HashSet<_> = planned.iter().map(|(key, _)| key.clone()).collect();
    for (mut key, file) in planned {
        // `increment` numbers outputs `x_1`, `x_2`, … which is exactly what
        // an input stem `x_1` produces. Run such inputs in the group of `x`,
        // so the names do not depend on which batch worker gets there first.
        if naming.policy == OverwritePolicy::Increment {
            while let Some(base) = numbered_base(&key.1) {
                let candidate = (key.0.clone(), base, key.2.clone());
                if !keys.contains(&candidate) {
                    break;
                }
                key = candidate;
            }
        }
        match index.get(&key) {
            Some(&group) => groups.get_mut(group).map_or((), |g: &mut Vec<_>| g.push(file.clone())),
            None => {
                index.insert(key, groups.len());
                groups.push(vec![file.clone()]);
            }
        }
    }
    let collisions: Vec<_> = groups.iter().filter(|group| group.len() > 1).collect();
    if !collisions.is_empty()
        && naming.policy != OverwritePolicy::Increment
        && !naming.per_image_suffix
    {
        let mut message = gettext("several inputs would write the same output file:");
        for group in collisions {
            let names: Vec<_> = group.iter().map(|path| path.display().to_string()).collect();
            message.push_str("\n  ");
            message.push_str(&names.join(", "));
        }
        message.push('\n');
        message.push_str(&gettext(
            "Rename the inputs, process them separately or use the increment policy.",
        ));
        return Err(color_eyre::eyre::eyre!(codec::sanitize_text(&message)));
    }
    Ok(groups)
}

/// `x` for a stem `x_N` as written by the `increment` policy (N >= 1, no
/// leading zero).
fn numbered_base(stem: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = stem.as_bytes();
    let split = bytes.iter().rposition(|&byte| byte == b'_')?;
    let number = &bytes[split + 1..];
    let numbered = !number.is_empty()
        && number[0] != b'0'
        && number.iter().all(u8::is_ascii_digit)
        && split > 0;
    numbered.then(|| std::ffi::OsString::from_vec(bytes[..split].to_vec()))
}

fn run_per_file<F>(
    files: &[PathBuf],
    op_name: &str,
    naming: Naming,
    mut op: F,
) -> color_eyre::Result<()>
where
    F: FnMut(&PathBuf) -> bigimage_core::Result<ConvertOutcome>,
{
    let groups = plan_outputs(files, naming)?;
    let mut written = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    // Outputs of this run, to catch per-image names that still collided.
    let mut outputs = std::collections::HashSet::new();

    for f in groups.iter().flatten() {
        let result = op(f);
        if bigimage_core::color::take_ignored_profile() {
            note_ignored_profile(f);
        }
        match result {
            Ok(ConvertOutcome::Written { output })
                if naming.per_image_suffix && !outputs.insert(output.clone()) =>
            {
                failed += 1;
                let output = codec::sanitize_text(&output.display().to_string());
                eprintln!(
                    "err {}: {}",
                    f.display(),
                    fill(
                        &gettext("replaced the output of another input: {path}"),
                        &[("path", &output)]
                    )
                );
            }
            Ok(ConvertOutcome::Written { output }) => {
                written += 1;
                println!("ok  {} -> {}", f.display(), output.display());
            }
            Ok(ConvertOutcome::Skipped { output }) => {
                skipped += 1;
                println!("skip {} ({})", f.display(), exists_note(&output));
            }
            Err(e) => {
                failed += 1;
                eprintln!("err {}: {}", f.display(), codec::sanitize_text(&e.to_string()));
            }
        }
    }

    eprintln!("{}", tally(op_name, written, skipped, failed));

    if failed > 0 {
        exit_partial();
    }
    Ok(())
}

/// Why a `skip` line wrote nothing.
/// The input's colour profile was malformed and its pixels were used as
/// sRGB; the conversion went on.
fn note_ignored_profile(input: &Path) {
    eprintln!(
        "info {}: {}",
        input.display(),
        gettext("the color profile is corrupted; colors were treated as sRGB")
    );
}

fn exists_note(output: &Path) -> String {
    fill(&gettext("already exists: {path}"), &[("path", &output.display().to_string())])
}

/// Closing line of a command that processed several files.
fn tally(operation: &str, written: usize, skipped: usize, failed: usize) -> String {
    fill(
        &gettext("{operation}: {written} written, {skipped} skipped, {failed} failed"),
        &[
            ("operation", operation),
            ("written", &written.to_string()),
            ("skipped", &skipped.to_string()),
            ("failed", &failed.to_string()),
        ],
    )
}

fn run_remove_bg(
    model: ModelArg,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    tracing::info!(?model, ?policy, count = files.len(), "remove-bg");
    require_model(&bigimage_ai::background::MODEL)?;
    let naming = Naming::image(Some(Format::Png), policy);
    let groups = plan_outputs(files, naming)?;

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut fail = 0usize;
    // Loaded on first use: skipping existing outputs needs no model.
    let mut slot = None;
    for f in groups.iter().flatten() {
        let dest = remove_bg_output(f);
        if policy == OverwritePolicy::Skip && dest.exists() {
            skipped += 1;
            println!("skip {} ({})", f.display(), exists_note(&dest));
            continue;
        }
        let session = match &mut slot {
            Some(session) => session,
            None => {
                let session = bigimage_ai::background::BgSession::new(|_| Continue(()))?;
                report_device("remove-bg", bigimage_ai::background::device());
                slot.insert(session)
            }
        };
        match remove_bg_one(f, dest, policy, session) {
            Ok(ConvertOutcome::Written { output }) => {
                ok += 1;
                println!("ok  {} -> {}", f.display(), output.display());
            }
            Ok(ConvertOutcome::Skipped { output }) => {
                skipped += 1;
                println!("skip {} ({})", f.display(), exists_note(&output));
            }
            Err(e) => {
                fail += 1;
                eprintln!("err {}: {}", f.display(), codec::sanitize_text(&e.to_string()));
            }
        }
    }
    eprintln!("{}", tally("remove-bg", ok, skipped, fail));
    if fail > 0 {
        exit_partial();
    }
    Ok(())
}

/// `<dir>/<stem>_nobg.png`, built from the raw OS name so non-UTF-8 stems
/// keep distinct outputs.
fn remove_bg_output(path: &Path) -> PathBuf {
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push("_nobg.png");
    path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")).join(name)
}

fn remove_bg_one(
    path: &Path,
    dest: PathBuf,
    policy: OverwritePolicy,
    session: &mut bigimage_ai::background::BgSession,
) -> color_eyre::Result<ConvertOutcome> {
    bigimage_core::content::ensure_static(path)?;
    let img =
        bigimage_core::open_image(path).map_err(|e| color_eyre::eyre::eyre!("decode: {e}"))?;
    let out_img = session.process(&img).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    bigimage_core::save_image(out_img, dest, Format::Png, &EncodeOptions::default(), policy)
        .map_err(|e| color_eyre::eyre::eyre!("encode: {e}"))
}

/// Refuse to start when `model` cannot run: not built in, or its weights
/// package is missing (the message names the package to install).
fn require_model(model: &bigimage_ai::ModelInfo) -> color_eyre::Result<()> {
    match model.availability() {
        bigimage_ai::Availability::Ready => Ok(()),
        bigimage_ai::Availability::NotBuilt => {
            Err(color_eyre::eyre::eyre!("{}", bigimage_ai::AiError::Disabled))
        }
        bigimage_ai::Availability::Missing { path } => Err(color_eyre::eyre::eyre!(fill(
            &gettext(
                "{model} is not installed ({path} does not exist). Install it with: {command}"
            ),
            &[
                ("model", model.name),
                ("path", &path.display().to_string()),
                ("command", &model.install_command()),
            ],
        ))),
        bigimage_ai::Availability::Unreadable { path, error } => {
            Err(color_eyre::eyre::eyre!(fill(
                &gettext("could not read the {model} weights at {path}: {error}"),
                &[
                    ("model", model.name),
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )))
        }
    }
}

/// Say once where a model runs (CPU or which GPU).
fn report_device(op: &str, device: Option<bigimage_ai::Device>) {
    if let Some(device) = device {
        eprintln!("{op}: {device}");
    }
}

fn ai_error(error: bigimage_ai::AiError) -> bigimage_core::BigImageError {
    bigimage_core::BigImageError::Other(error.to_string())
}

/// Upscale by 2, 3 or 4: Real-ESRGAN when installed (or asked for), else
/// Lanczos3. Output `<stem>_<pct>pct.<ext>` either way, as before.
fn run_upscale(
    factor: u8,
    engine: UpscaleEngineArg,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    use bigimage_ai::upscale::Method;
    if !(2..=4).contains(&factor) {
        return Err(color_eyre::eyre::eyre!(fill(
            &gettext("invalid factor: {factor}. Use 2, 3 or 4."),
            &[("factor", &factor.to_string())],
        )));
    }
    let method = match engine {
        UpscaleEngineArg::Lanczos => Method::Lanczos,
        UpscaleEngineArg::Realesrgan => {
            require_model(&bigimage_ai::upscale::MODEL)?;
            Method::RealEsrgan
        }
        UpscaleEngineArg::Auto => {
            let method = Method::auto();
            if method == Method::Lanczos {
                eprintln!(
                    "upscale: {}",
                    fill(
                        &gettext("Real-ESRGAN unavailable; using Lanczos3. To use AI: {command}"),
                        &[("command", &bigimage_ai::upscale::MODEL.install_command())],
                    )
                );
            }
            method
        }
    };
    tracing::info!(factor, ?method, ?policy, count = files.len(), "upscale");
    let opts = bigimage_core::EncodeOptions::default();
    if method == Method::Lanczos {
        let mode = ResizeMode::Percent(f32::from(factor) * 100.0);
        return run_per_file(files, "upscale", Naming::image(None, policy), |f| {
            bigimage_core::resize_file(f, mode, Filter::Lanczos3, None, &opts, policy)
        });
    }
    let suffix = format!("_{}pct", u32::from(factor) * 100);
    let mut reported = false;
    run_per_file(files, "upscale", Naming::image(None, policy), |f| {
        let outcome = bigimage_core::transform_file(f, &suffix, None, &opts, policy, |img| {
            bigimage_ai::upscale::upscale(&img, factor, method, |_| Continue(())).map_err(ai_error)
        });
        if !reported && bigimage_ai::upscale::device().is_some() {
            reported = true;
            report_device("upscale", bigimage_ai::upscale::device());
        }
        outcome
    })
}

/// Encoding for denoise and inpaint, which keep the source's format: a
/// lossy one is re-encoded, at the target closest to the source.
fn retouch_options() -> EncodeOptions {
    EncodeOptions { target: QualityTarget::Maximum, ..EncodeOptions::default() }
}

/// Remove noise with SCUNet. Output `<stem>_denoised.<ext>`.
fn run_denoise(policy: OverwritePolicy, files: &[PathBuf]) -> color_eyre::Result<()> {
    require_model(&bigimage_ai::denoise::MODEL)?;
    tracing::info!(?policy, count = files.len(), "denoise");
    let opts = retouch_options();
    let mut reported = false;
    run_per_file(files, "denoise", Naming::image(None, policy), |f| {
        let outcome = bigimage_core::transform_file(f, "_denoised", None, &opts, policy, |img| {
            bigimage_ai::denoise::denoise(&img, |_| Continue(())).map_err(ai_error)
        });
        if !reported && bigimage_ai::denoise::device().is_some() {
            reported = true;
            report_device("denoise", bigimage_ai::denoise::device());
        }
        outcome
    })
}

/// Fill the white area of `mask` with LaMa. Output `<stem>_inpaint.<ext>`.
fn run_inpaint(mask: &Path, policy: OverwritePolicy, files: &[PathBuf]) -> color_eyre::Result<()> {
    require_model(&bigimage_ai::inpaint::MODEL)?;
    let mask_error = |e: &dyn std::fmt::Display| {
        color_eyre::eyre::eyre!(fill(
            &gettext("mask {path}: {error}"),
            &[
                ("path", &codec::sanitize_text(&mask.display().to_string())),
                ("error", &codec::sanitize_text(&e.to_string())),
            ],
        ))
    };
    bigimage_core::content::ensure_static(mask).map_err(|e| mask_error(&e))?;
    let mask_image = bigimage_core::open_image(mask).map_err(|e| mask_error(&e))?;
    let mask_image = bigimage_ai::inpaint::mask_from_image(&mask_image);
    if bigimage_ai::inpaint::plans(&mask_image).map_err(|e| mask_error(&e))?.is_empty() {
        return Err(mask_error(&gettext("marks no area (paint the areas to erase white)")));
    }
    tracing::info!(?mask, ?policy, count = files.len(), "inpaint");
    let opts = retouch_options();
    let mut reported = false;
    run_per_file(files, "inpaint", Naming::image(None, policy), |f| {
        let outcome = bigimage_core::transform_file(f, "_inpaint", None, &opts, policy, |img| {
            bigimage_ai::inpaint::inpaint(&img, &mask_image, |_| Continue(())).map_err(ai_error)
        });
        if !reported && bigimage_ai::inpaint::device().is_some() {
            reported = true;
            report_device("inpaint", bigimage_ai::inpaint::device());
        }
        outcome
    })
}

fn run_animate(
    output: &std::path::Path,
    delay_ms: u32,
    loop_count: u16,
    speed: i32,
    policy: OverwritePolicy,
    frames: &[PathBuf],
) -> color_eyre::Result<()> {
    let loop_mode = match loop_count {
        0 => LoopMode::Infinite,
        1 => LoopMode::Once,
        n => LoopMode::Finite(n),
    };
    let opts = AnimateOptions { delay_ms, loop_mode, speed };
    tracing::info!(?output, ?opts, ?policy, frames = frames.len(), "animate");
    match bigimage_core::make_gif_with_policy(frames, output, opts, policy) {
        Ok(ConvertOutcome::Written { output }) => {
            let count = fill(&gettext("{count} frame(s)"), &[("count", &frames.len().to_string())]);
            println!("ok  {count} -> {}", output.display());
            Ok(())
        }
        Ok(ConvertOutcome::Skipped { output }) => {
            println!("skip animate ({})", exists_note(&output));
            Ok(())
        }
        // Frames were read but could not be turned into the animation.
        Err(e) => {
            eprintln!("err animate: {}", codec::sanitize_text(&e.to_string()));
            exit_partial();
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchProfile {
    /// Target extension/name accepted by `Format::parse`.
    format: String,
    quality: Option<u8>,
    /// maximum | balanced | smallest (or max | small).
    target: Option<String>,
    #[serde(default)]
    progressive: bool,
    #[serde(default)]
    optimize: bool,
    #[serde(default = "default_batch_overwrite")]
    overwrite: String,
    output_dir: Option<PathBuf>,
}

fn default_batch_overwrite() -> String {
    "skip".to_string()
}

fn unreadable(path: &Path, error: &std::io::Error) -> color_eyre::Report {
    color_eyre::eyre::eyre!(fill(
        &gettext("could not read {path}: {error}"),
        &[("path", &path.display().to_string()), ("error", &error.to_string())],
    ))
}

/// The files named on the command line, and the supported images directly inside named folders.
fn batch_files(inputs: &[PathBuf]) -> color_eyre::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let entries = std::fs::read_dir(input).map_err(|e| unreadable(input, &e))?;
            files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| Format::parse(ext).is_ok())
            }));
        } else {
            files.push(input.clone());
        }
    }
    Ok(files)
}

fn run_batch(profile: &std::path::Path, inputs: &[PathBuf]) -> color_eyre::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    let text = std::fs::read_to_string(profile).map_err(|e| unreadable(profile, &e))?;
    let profile: BatchProfile = toml::from_str(&text).map_err(|e| {
        color_eyre::eyre::eyre!(with_error(&gettext("invalid TOML profile: {error}"), e))
    })?;
    if profile.quality.is_some_and(|quality| !(1..=100).contains(&quality)) {
        return Err(color_eyre::eyre::eyre!(gettext("quality must be between 1 and 100")));
    }
    let format = Format::parse(&profile.format).map_err(|e| {
        color_eyre::eyre::eyre!(with_error(&gettext("invalid profile format: {error}"), e))
    })?;
    let target = match profile.target.as_deref() {
        None => QualityTarget::default(),
        Some(value) => parse_target(value).map_err(|e| color_eyre::eyre::eyre!("{e}"))?,
    };
    let policy = match profile.overwrite.trim().to_ascii_lowercase().as_str() {
        "skip" => OverwritePolicy::Skip,
        "replace" => OverwritePolicy::Replace,
        "increment" => OverwritePolicy::Increment,
        other => {
            return Err(color_eyre::eyre::eyre!(fill(
                &gettext("invalid overwrite: {value}; use skip, replace or increment"),
                &[("value", &format!("{other:?}"))],
            )));
        }
    };
    if let Some(dir) = &profile.output_dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            color_eyre::eyre::eyre!(fill(
                &gettext("could not create {path}: {error}"),
                &[("path", &dir.display().to_string()), ("error", &e.to_string())],
            ))
        })?;
    }

    let mut files = batch_files(inputs)?;
    files.sort();
    if files.is_empty() {
        return Err(color_eyre::eyre::eyre!(gettext("no images found for the batch")));
    }
    let naming = Naming {
        output_dir: profile.output_dir.as_deref(),
        extension: Some(format.extension()),
        policy,
        per_image_suffix: false,
    };
    // Each group shares one destination and runs in order on one worker,
    // which keeps `increment` numbering deterministic.
    let groups = plan_outputs(&files, naming)?;

    let total: usize = groups.iter().map(Vec::len).sum();
    let opts = EncodeOptions {
        quality: profile.quality,
        progressive: profile.progressive,
        optimize: profile.optimize,
        target,
        ..EncodeOptions::default()
    };
    let flat: Vec<PathBuf> = groups.iter().flatten().cloned().collect();
    let worker_count = bigimage_core::resources::batch_worker_count(&flat).min(groups.len());
    let groups = Arc::new(groups);
    let output_dir = Arc::new(profile.output_dir);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    tracing::info!(?format, ?policy, total, worker_count, "batch");

    let workers: Vec<_> = (0..worker_count)
        .map(|_| {
            let groups = groups.clone();
            let output_dir = output_dir.clone();
            let next = next.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    let Some(group) = groups.get(idx) else { break };
                    for input in group {
                        let result = bigimage_core::convert_file_to(
                            input,
                            output_dir.as_deref(),
                            format,
                            &opts,
                            policy,
                        )
                        .map_err(|e| codec::sanitize_text(&e.to_string()));
                        let ignored_profile = bigimage_core::color::take_ignored_profile();
                        if tx.send((input.clone(), result, ignored_profile)).is_err() {
                            return;
                        }
                    }
                }
            })
        })
        .collect();
    drop(tx);

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for (done, (input, result, ignored_profile)) in rx.into_iter().enumerate() {
        if ignored_profile {
            note_ignored_profile(&input);
        }
        match result {
            Ok(ConvertOutcome::Written { output }) => {
                ok += 1;
                println!(
                    "[{}/{}] ok   {} -> {}",
                    done + 1,
                    total,
                    input.display(),
                    output.display()
                );
            }
            Ok(ConvertOutcome::Skipped { output }) => {
                skipped += 1;
                println!("[{}/{}] skip {}", done + 1, total, output.display());
            }
            Err(error) => {
                failed += 1;
                eprintln!("[{}/{}] error {}: {error}", done + 1, total, input.display());
            }
        }
    }
    for worker in workers {
        if worker.join().is_err() {
            failed += 1;
            eprintln!("err batch: {}", gettext("a worker ended unexpectedly"));
        }
    }
    println!("{}", tally("batch", ok, skipped, failed));
    if failed > 0 {
        exit_partial();
    }
    Ok(())
}

/// Convert each file to PDF with `soffice --headless --convert-to pdf`, next
/// to its source: the office formats LibreOffice reads, which BigOCRPDF
/// (images and existing PDFs only) does not cover.
fn run_to_pdf(
    files: &[PathBuf],
    policy: OverwritePolicy,
    timeout: std::time::Duration,
) -> color_eyre::Result<()> {
    tracing::info!(?policy, ?timeout, count = files.len(), "to-pdf");
    // Checked once, so a missing LibreOffice is one error, not one per file.
    let soffice_path = which_soffice().ok_or_else(|| {
        color_eyre::eyre::eyre!(gettext(
            "LibreOffice (`soffice`) not found in PATH. Install `libreoffice-fresh` (recommended) or `libreoffice-still` (e.g. `sudo pacman -S libreoffice-fresh`)."
        ))
    })?;
    let naming =
        Naming { output_dir: None, extension: Some("pdf"), policy, per_image_suffix: false };
    let groups = plan_outputs(files, naming)?;

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut fail = 0usize;
    for file in groups.iter().flatten() {
        match to_pdf_one(&soffice_path, file, policy, timeout) {
            Ok(ConvertOutcome::Written { output }) => {
                ok += 1;
                println!("ok  {} -> {}", file.display(), output.display());
            }
            Ok(ConvertOutcome::Skipped { output }) => {
                skipped += 1;
                println!("skip {} ({})", file.display(), exists_note(&output));
            }
            Err(e) => {
                fail += 1;
                eprintln!("err {}: {}", file.display(), codec::sanitize_text(&e.to_string()));
            }
        }
    }
    eprintln!("{}", tally("to-pdf", ok, skipped, fail));
    if fail > 0 {
        exit_partial();
    }
    Ok(())
}

/// Convert one document in a private LibreOffice profile and a private
/// output folder, then publish the PDF with the overwrite policy.
///
/// soffice exits 0 when it cannot load a document or when another instance
/// owns the profile, so success means "a PDF exists in the private
/// staging folder", not the status.
fn to_pdf_one(
    soffice: &Path,
    file: &Path,
    policy: OverwritePolicy,
    timeout: std::time::Duration,
) -> color_eyre::Result<ConvertOutcome> {
    use color_eyre::eyre::eyre;
    use std::os::unix::process::CommandExt;
    // Absolute paths: a relative name such as `-doc.txt` would be an option.
    // Not canonical: a symlink keeps its own name and folder for the PDF,
    // matching how the outputs were planned.
    let file = std::path::absolute(file).map_err(|e| eyre!("{e}"))?;
    if !file.is_file() {
        return Err(eyre!(gettext("not a regular file")));
    }
    let dir = file.parent().unwrap_or(Path::new("/"));
    let stem = file.file_stem().ok_or_else(|| eyre!(gettext("invalid file name")))?;
    let mut name = stem.to_os_string();
    name.push(".pdf");
    let destination = dir.join(&name);
    if policy == OverwritePolicy::Skip && destination.exists() {
        return Ok(ConvertOutcome::Skipped { output: destination });
    }
    // Same folder as the destination, so publishing is an atomic rename.
    let staging = tempfile::Builder::new().prefix(".bigiris-pdf-").tempdir_in(dir)?;
    let profile = tempfile::Builder::new().prefix("bigiris-soffice-").tempdir()?;
    let mut child = codec::close_inherited_descriptors(&mut std::process::Command::new(soffice))
        .arg(format!("-env:UserInstallation={}", file_url(profile.path())))
        .args(["--headless", "--norestore", "--nolockcheck", "--convert-to", "pdf", "--outdir"])
        .arg(staging.path())
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // soffice forks soffice.bin; a group lets the timeout reach both.
        .process_group(0)
        .spawn()
        .map_err(|e| eyre!(with_error(&gettext("could not run soffice: {error}"), e)))?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let group = rustix::process::Pid::from_child(&child);
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            let _ = child.wait();
            return Err(eyre!(fill(
                &gettext("soffice exceeded the {seconds} s limit"),
                &[("seconds", &timeout.as_secs().to_string())],
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let produced = staging.path().join(&name);
    // The staging folder is new and private, so anything there is fresh;
    // no mtime comparison, which clock skew on NFS/SMB would break.
    let produced_pdf =
        std::fs::symlink_metadata(&produced).is_ok_and(|meta| meta.is_file() && meta.len() > 0);
    if !produced_pdf {
        return Err(eyre!(fill(
            &gettext("soffice did not produce the PDF ({status})"),
            &[("status", &status.to_string())],
        )));
    }
    publish_file(&produced, destination, policy)
}

/// Move `source` to `destination` honouring the policy without ever
/// clobbering an existing file for `skip` and `increment`.
fn publish_file(
    source: &Path,
    destination: PathBuf,
    policy: OverwritePolicy,
) -> color_eyre::Result<ConvertOutcome> {
    if policy == OverwritePolicy::Replace {
        std::fs::rename(source, &destination)?;
        return Ok(ConvertOutcome::Written { output: destination });
    }
    let mut temp = tempfile::TempPath::try_from_path(source)?;
    for n in 0..=9_999u32 {
        let candidate = if n == 0 {
            destination.clone()
        } else {
            let mut name = destination.file_stem().unwrap_or_default().to_os_string();
            name.push(format!("_{n}"));
            if let Some(ext) = destination.extension() {
                name.push(".");
                name.push(ext);
            }
            destination.with_file_name(name)
        };
        match temp.persist_noclobber(&candidate) {
            Ok(()) => return Ok(ConvertOutcome::Written { output: candidate }),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if policy == OverwritePolicy::Skip {
                    return Ok(ConvertOutcome::Skipped { output: destination });
                }
                temp = e.path;
            }
            Err(e) => return Err(e.error.into()),
        }
    }
    Err(color_eyre::eyre::eyre!(fill(
        &gettext("too many files with the same name: {path}"),
        &[("path", &destination.display().to_string())],
    )))
}

/// `file://` URL for a local path, percent-encoding everything but RFC 3986
/// unreserved characters and `/`.
fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// `soffice`, LibreOffice's command name on every distribution, from `PATH`.
fn which_soffice() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("soffice");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_install(user: bool, system: bool, destdir: Option<PathBuf>) -> color_eyre::Result<()> {
    let report = match destdir {
        Some(dir) => {
            tracing::info!(destdir = %dir.display(), "install-integrations (destdir)");
            bigimage_integrations::install_to_destdir(&dir)
        }
        None => {
            let scope = resolve_scope(user, system);
            tracing::info!(?scope, "install-integrations");
            bigimage_integrations::install(scope).map_err(|e| {
                color_eyre::eyre::eyre!(with_error(&gettext("installation failed: {error}"), e))
            })?
        }
    };
    print_install_report(
        &report,
        &gettext("installed"),
        &gettext("installed in total: {files} file(s) in {managers} file manager(s)"),
    );
    if report.is_success() {
        Ok(())
    } else {
        exit_partial();
    }
}

fn run_uninstall() -> color_eyre::Result<()> {
    // Uninstall always runs against the user scope for now; a system-level
    // uninstall would require root and typically the distro package handles
    // it via `makepkg` / `.install` hooks.
    let scope = bigimage_integrations::Scope::User;
    tracing::info!(?scope, "uninstall-integrations");
    let report = bigimage_integrations::uninstall(scope)
        .map_err(|e| color_eyre::eyre::eyre!(with_error(&gettext("removal failed: {error}"), e)))?;
    print_install_report(
        &report,
        &gettext("removed"),
        &gettext("removed in total: {files} file(s) in {managers} file manager(s)"),
    );
    if report.is_success() {
        Ok(())
    } else {
        exit_partial();
    }
}

fn resolve_scope(user: bool, system: bool) -> bigimage_integrations::Scope {
    let _ = user; // User is the default; the flag exists for clarity.
    if system { bigimage_integrations::Scope::System } else { bigimage_integrations::Scope::User }
}

/// `done` is the status of a manager whose files were written or removed;
/// `summary` takes `{files}` and `{managers}`.
fn print_install_report(report: &bigimage_integrations::Report, done: &str, summary: &str) {
    for outcome in &report.outcomes {
        let status = if outcome.error.is_some() {
            gettext("FAILED")
        } else if outcome.packaged {
            // The package ships it system-wide: nothing personal was written
            // and stale copies from earlier user installs were removed.
            // Without this it read "unchanged", as if install were a no-op.
            gettext("provided by the package")
        } else if outcome.files.is_empty() {
            if outcome.detected { gettext("unchanged") } else { gettext("skipped") }
        } else {
            done.to_owned()
        };
        let detected = if outcome.detected { gettext("detected") } else { gettext("not in PATH") };
        let files =
            fill(&gettext("{count} file(s)"), &[("count", &format!("{:>2}", outcome.files.len()))]);
        println!("  {:<22}  [{status}]  {files}  ({detected})", outcome.fm.display_name());
        if let Some(err) = &outcome.error {
            eprintln!("    -> {err}");
        }
    }
    eprintln!(
        "{}",
        fill(
            summary,
            &[
                ("files", &report.files_touched().to_string()),
                ("managers", &report.outcomes.len().to_string()),
            ],
        )
    );
}

fn dispatch_gui(dialog: Option<Dialog>, files: &[PathBuf]) -> color_eyre::Result<()> {
    #[cfg(not(feature = "gui"))]
    {
        if let Some(d) = dialog {
            eprintln!(
                "{}",
                fill(
                    &gettext(
                        "warning: this build lacks the `gui` feature. Dialog '{dialog}' ignored (rebuild with --features gui)."
                    ),
                    &[("dialog", &format!("{d:?}"))],
                )
            );
        } else if files.is_empty() {
            eprintln!(
                "{}",
                gettext(
                    "warning: this build lacks the `gui` feature. Viewer unavailable (rebuild with --features gui)."
                )
            );
        } else {
            eprintln!(
                "{}",
                gettext(
                    "warning: this build lacks the `gui` feature. Viewer unavailable; files received:"
                )
            );
            for f in files {
                eprintln!("  - {}", f.display());
            }
        }
        Ok(())
    }

    #[cfg(feature = "gui")]
    {
        // Leave one core to the desktop while a model runs (the CLI uses
        // them all).
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        bigimage_ai::set_threads(cores.saturating_sub(1).max(1));
        let code = match dialog {
            Some(Dialog::Convert) => gui::run_convert_dialog(files.to_vec()),
            Some(Dialog::Resize) => gui::run_resize_dialog(files.to_vec()),
            Some(Dialog::Rotate) => gui::run_rotate_dialog(files.to_vec()),
            Some(Dialog::Flip) => gui::run_flip_dialog(files.to_vec()),
            Some(Dialog::Adjust) => gui::run_adjust_dialog(files.to_vec()),
            Some(Dialog::Metadata) => gui::run_metadata_dialog(files.to_vec()),
            Some(Dialog::Compare) => gui::run_compare_dialog(files.to_vec()),
            Some(Dialog::Animate) => gui::run_animate_dialog(files.to_vec()),
            Some(Dialog::RemoveBg) => gui::run_remove_bg_dialog(files.to_vec()),
            Some(Dialog::Crop) => gui::run_crop_dialog(files.to_vec()),
            Some(Dialog::Extent) => gui::run_extent_dialog(files.to_vec()),
            Some(Dialog::Upscale) => gui::run_upscale_dialog(files.to_vec()),
            Some(Dialog::Batch) => gui::run_batch_dialog(files.to_vec()),
            None => gui::run_viewer(files.to_vec()),
        };
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_groups_numbered_stems_with_their_base() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["x_1.png", "IMG_2.png", "x.png", "x_1_1.png", "x_01.png", "_1.png"];
        let files: Vec<_> = names.iter().map(|name| dir.path().join(name)).collect();
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }
        let naming = Naming::image(Some(Format::Png), OverwritePolicy::Increment);
        let groups = plan_outputs(&files, naming).unwrap();
        let named: Vec<Vec<_>> = groups
            .iter()
            .map(|group| group.iter().map(|path| path.file_name().unwrap()).collect())
            .collect();
        // `x_1_1` -> `x_1` -> `x`; IMG_2 has no `IMG` input; `x_01` is no counter.
        let expected = [
            vec!["x_1.png", "x.png", "x_1_1.png"],
            vec!["IMG_2.png"],
            vec!["x_01.png"],
            vec!["_1.png"],
        ];
        assert_eq!(named, expected);
        // Other policies never number outputs, so nothing is merged.
        let naming = Naming::image(Some(Format::Png), OverwritePolicy::Skip);
        assert_eq!(plan_outputs(&files, naming).unwrap().len(), names.len());
    }
}
