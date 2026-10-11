// SPDX-License-Identifier: GPL-3.0-or-later
//! BigIris — modern image viewer and converter for Linux.
//!
//! Single binary, three entry modes:
//! 1. Viewer: `bigiris [FILE...]`
//! 2. Dialog: `bigiris --dialog=<name> [FILE...]` (used by file-manager service menus)
//! 3. Headless CLI: `bigiris <subcommand> ...` (used by CI/CD and quick service-menu actions)

#![forbid(unsafe_code)]

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

use std::path::PathBuf;

use bigimage_core::{
    AdjustOps, AnimateOptions, ConvertOutcome, CropRect, EncodeOptions, ExtentOptions, ExtentSize,
    Filter, FlipAxis, Format, Gravity, LoopMode, OverwritePolicy, ResizeMode, Rotation,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "bigiris",
    version,
    about = "Visualizador rápido e conversor de imagens para Linux",
    long_about = None,
)]
struct Cli {
    /// Abrir apenas um diálogo modal específico (usado por service menus)
    #[arg(long, value_name = "NAME")]
    dialog: Option<Dialog>,

    /// Rodar smoke-test e sair (usado em CI e pós-install)
    #[arg(long)]
    self_test: bool,

    /// Arquivos a abrir no visualizador (quando sem subcomando)
    #[arg(value_name = "FILE")]
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
    /// Não sobrescreve o arquivo existente (default).
    #[default]
    Skip,
    /// Sobrescreve o arquivo existente.
    Replace,
    /// Gera nome incrementado (foo_1.png, foo_2.png, ...).
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
    /// Aresta máxima em pixels (preserva aspecto; no-op se a imagem já é menor).
    #[arg(long = "max-edge")]
    max_edge: Option<u32>,
    /// Escala percentual (ex: 50 para metade; 200 para o dobro).
    #[arg(long)]
    percent: Option<f32>,
    /// Dimensão exata WxH (ignora aspecto).
    #[arg(long, value_name = "WxH")]
    exact: Option<WxH>,
    /// Encaixa dentro da caixa WxH preservando aspecto.
    #[arg(long, value_name = "WxH")]
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
            _ => unreachable!("clap ArgGroup garante exatamente um modo selecionado"),
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
        let (w, h) = s
            .split_once(['x', 'X'])
            .ok_or_else(|| format!("esperado formato WxH (ex. 1920x1080), recebido: {s:?}"))?;
        let width: u32 = w.parse().map_err(|e| format!("largura inválida: {e}"))?;
        let height: u32 = h.parse().map_err(|e| format!("altura inválida: {e}"))?;
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
        let (wh, rest) =
            s.split_once('+').ok_or_else(|| format!("esperado WxH+X+Y, recebido: {s:?}"))?;
        let (x, y) = rest.split_once('+').ok_or_else(|| format!("faltou o offset Y em {s:?}"))?;
        let (w, h) =
            wh.split_once(['x', 'X']).ok_or_else(|| format!("faltou largura/altura em {s:?}"))?;
        let width: u32 = w.parse().map_err(|e| format!("largura inválida: {e}"))?;
        let height: u32 = h.parse().map_err(|e| format!("altura inválida: {e}"))?;
        let x: u32 = x.parse().map_err(|e| format!("offset X inválido: {e}"))?;
        let y: u32 = y.parse().map_err(|e| format!("offset Y inválido: {e}"))?;
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
        let (a, b) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("esperado W:H (ex. 1:1, 9:16), recebido: {s:?}"))?;
        let w: u32 = a.trim().parse().map_err(|e| format!("razão W inválido: {e}"))?;
        let h: u32 = b.trim().parse().map_err(|e| format!("razão H inválido: {e}"))?;
        if w == 0 || h == 0 {
            return Err(format!("razão com componente zero não faz sentido: {s:?}"));
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
    /// Tamanho exato do canvas em pixels (WxH).
    #[arg(long, value_name = "WxH")]
    size: Option<WxH>,
    /// Razão alvo (W:H). O canvas é estendido o mínimo necessário para
    /// caber a imagem nessa proporção.
    #[arg(long, value_name = "W:H")]
    aspect: Option<Aspect>,
}

impl ExtentSizeArgs {
    fn into_size(self) -> ExtentSize {
        match self {
            Self { size: Some(d), .. } => ExtentSize::Exact { width: d.width, height: d.height },
            Self { aspect: Some(a), .. } => ExtentSize::Aspect { w: a.w, h: a.h },
            _ => unreachable!("clap ArgGroup garante exatamente um modo selecionado"),
        }
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Converter arquivos (headless)
    Convert {
        /// Formato de destino: png, jpg, webp, avif, tiff, bmp, gif,
        /// ico, pnm/ppm/pgm/pbm, tga, qoi, hdr, exr
        #[arg(long)]
        to: String,
        /// Qualidade 1..100 (afeta JPEG/lossy). Omitido usa o default do encoder.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: Option<u8>,
        /// JPEG progressivo (aceito para compatibilidade futura).
        #[arg(long)]
        progressive: bool,
        /// Otimizar tamanho: PNG usa melhor compressao, strip de metadata extra.
        #[arg(long)]
        optimize: bool,
        /// Política quando o arquivo de saída já existe
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        /// Arquivos de entrada
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Redimensionar arquivos (headless). Exatamente um modo entre
    /// --max-edge / --percent / --exact / --fit é obrigatório.
    Resize {
        #[command(flatten)]
        mode: ResizeModeArgs,
        /// Kernel de interpolação (default: lanczos3).
        #[arg(long, value_enum, default_value_t = FilterArg::Lanczos3)]
        filter: FilterArg,
        /// Formato de destino (opcional; mantém o formato original se omitido).
        #[arg(long)]
        to: Option<String>,
        /// Qualidade 1..100 (afeta JPEG/lossy).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: Option<u8>,
        /// JPEG progressivo (aceito para compatibilidade futura).
        #[arg(long)]
        progressive: bool,
        /// Otimizar tamanho do PNG de saída.
        #[arg(long)]
        optimize: bool,
        /// Política quando o arquivo de saída já existe.
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Girar arquivos (headless). Rotações cardinais via pipeline de re-encode;
    /// `--auto` lê o tag EXIF Orientation e aplica a rotação certa.
    Rotate {
        /// Graus cardinais: 90 | 180 | 270.
        #[arg(long, value_enum, conflicts_with = "auto")]
        degrees: Option<RotationArg>,
        /// Auto-orientar via EXIF (tag Orientation).
        #[arg(long, conflicts_with = "degrees")]
        auto: bool,
        /// Formato de destino (opcional; mantém o formato original se omitido).
        #[arg(long)]
        to: Option<String>,
        /// Política quando o arquivo de saída já existe.
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Espelhar arquivos na horizontal ou vertical (headless).
    Flip {
        /// Eixo: horizontal | vertical.
        #[arg(long, value_enum)]
        axis: FlipAxisArg,
        /// Formato de destino (opcional).
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Recortar retângulo das imagens (headless). Use --rect WxH+X+Y ao estilo ImageMagick.
    Crop {
        /// Geometria do recorte no formato WxH+X+Y (ex: 800x600+100+50).
        #[arg(long)]
        rect: CropGeom,
        /// Formato de destino (opcional).
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Estender a tela em volta da imagem (letterbox). Exatamente um modo
    /// entre --size e --aspect é obrigatório. O canvas é preenchido com
    /// --bg e a imagem original é posicionada por --gravity.
    Extent {
        #[command(flatten)]
        size: ExtentSizeArgs,
        /// Cor de fundo em hexadecimal (#RRGGBB ou #RRGGBBAA).
        #[arg(long, default_value = "#000000")]
        bg: String,
        /// Posição da imagem original no canvas estendido.
        #[arg(long, value_enum, default_value_t = GravityArg::Center)]
        gravity: GravityArg,
        /// Formato de destino (opcional).
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Ajustar cor/tom (brilho, contraste, saturação, gamma).
    Adjust {
        /// Brilho -100..100 (0 = sem mudança).
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        brightness: i32,
        /// Contraste -100..100 (0 = sem mudança).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        contrast: f32,
        /// Saturação -100..100 (-100 = cinza, 0 = sem mudança).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        saturation: f32,
        /// Gamma 0.1..10.0 (< 1 clareia midtones, > 1 escurece).
        #[arg(long, default_value_t = 1.0)]
        gamma: f32,
        /// Formato de destino (opcional).
        #[arg(long)]
        to: Option<String>,
        #[arg(long = "overwrite", value_enum, default_value_t = OverwriteArg::Skip)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Remover fundo usando IA (headless)
    RemoveBg {
        /// Modelo: birefnet-lite | birefnet | u2net | u2netp
        #[arg(long, default_value = "birefnet-lite")]
        model: String,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Aumentar resolução (Lanczos3 CPU). Futuro backend IA (Real-ESRGAN)
    /// entra por --engine quando o modelo estiver disponível.
    Upscale {
        /// Fator inteiro (2, 3 ou 4).
        #[arg(long, default_value_t = 2)]
        factor: u8,
        /// Política de sobrescrita do arquivo de saída.
        #[arg(long, default_value_t, value_enum)]
        overwrite: OverwriteArg,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Criar GIF animado a partir de uma sequência de imagens (headless).
    Animate {
        /// Arquivo de saída (.gif).
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// Atraso entre quadros em milissegundos (100 ≈ 10 fps).
        #[arg(long, default_value_t = 100)]
        delay: u32,
        /// Número de repetições: 0 = infinito (default), 1+ = fixo.
        #[arg(long, default_value_t = 0)]
        loop_count: u16,
        /// Velocidade do encoder 1..30 (maior = mais rápido, pior paleta).
        #[arg(long, default_value_t = 10)]
        speed: i32,
        /// Quadros em ordem (mínimo 1).
        #[arg(value_name = "FRAME", required = true)]
        frames: Vec<PathBuf>,
    },
    /// Processar arquivos ou pastas em paralelo a partir de um perfil TOML
    Batch {
        /// Arquivo de perfil .toml
        #[arg(long)]
        profile: PathBuf,
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Instalar integrações com gerenciadores de arquivos
    InstallIntegrations {
        /// Instalar em ~/.local/share (default)
        #[arg(long, conflicts_with = "system")]
        user: bool,
        /// Instalar em /usr/share (requer root; normalmente feito pelo pacote)
        #[arg(long)]
        system: bool,
        /// Prefixo DESTDIR para empacotamento (ex: $pkgdir no PKGBUILD).
        /// Só faz sentido combinado com --system.
        #[arg(long, value_name = "PATH", requires = "system", conflicts_with = "user")]
        destdir: Option<PathBuf>,
    },
    /// Remover integrações previamente instaladas
    UninstallIntegrations,
    /// Converter documentos (ODT, DOCX, TXT, MD, RTF, XLSX, PPTX, etc.)
    /// para PDF via LibreOffice headless. Saída gravada ao lado do
    /// arquivo de origem.
    ToPdf {
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
}

fn main() -> color_eyre::Result<()> {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--codec-worker") {
        return codec::worker();
    }
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bigiris=info,bigimage_core=info".into()),
        )
        .init();

    init_i18n();

    let cli = Cli::parse();

    codec::configure();

    if cli.self_test {
        return self_test();
    }

    match cli.command {
        Some(Command::Convert { to, quality, progressive, optimize, overwrite, files }) => {
            let opts = EncodeOptions { quality, progressive, optimize, ..EncodeOptions::default() };
            run_convert(&to, opts, overwrite.into(), &files)
        }
        Some(Command::Resize {
            mode,
            filter,
            to,
            quality,
            progressive,
            optimize,
            overwrite,
            files,
        }) => {
            let opts = EncodeOptions { quality, progressive, optimize, ..EncodeOptions::default() };
            run_resize(
                mode.into_mode(),
                filter.into(),
                to.as_deref(),
                opts,
                overwrite.into(),
                &files,
            )
        }
        Some(Command::Rotate { degrees, auto, to, overwrite, files }) => {
            let target = parse_optional_target(to.as_deref())?;
            let policy: OverwritePolicy = overwrite.into();
            if auto {
                tracing::info!(?target, ?policy, count = files.len(), "rotate auto (EXIF)");
                run_per_file(&files, "rotate", |f| {
                    bigimage_core::rotate_file_auto(f, target, policy)
                })
            } else {
                let rotation: Rotation = degrees
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!("rotate: use --degrees 90|180|270 ou --auto")
                    })?
                    .into();
                tracing::info!(?rotation, ?target, ?policy, count = files.len(), "rotate");
                run_per_file(&files, "rotate", |f| {
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
        Some(Command::RemoveBg { model, files }) => run_remove_bg(&model, &files),
        Some(Command::Upscale { factor, overwrite, files }) => {
            run_upscale(factor, overwrite.into(), &files)
        }
        Some(Command::Animate { output, delay, loop_count, speed, frames }) => {
            run_animate(&output, delay, loop_count, speed, &frames)
        }
        Some(Command::Batch { profile, files }) => run_batch(&profile, &files),
        Some(Command::InstallIntegrations { user, system, destdir }) => {
            run_install(user, system, destdir)
        }
        Some(Command::UninstallIntegrations) => run_uninstall(),
        Some(Command::ToPdf { files }) => run_to_pdf(&files),
        None => dispatch_gui(cli.dialog, &cli.files),
    }
}

/// Bind the `bigiris` textdomain to `/usr/share/locale` (the system path
/// where the PKGBUILD installs compiled `.mo` catalogs) and switch to the
/// user's locale. Each call returns `Result`; any failure means gettext
/// silently returns source strings — acceptable graceful degrade for a
/// dev `cargo run` where no catalogs are installed yet.
fn init_i18n() {
    // GTK initializes the user locale during startup, before GUI workers.
    let _ = gettextrs::bindtextdomain("bigiris", "/usr/share/locale");
    let _ = gettextrs::bind_textdomain_codeset("bigiris", "UTF-8");
    let _ = gettextrs::textdomain("bigiris");
}

fn self_test() -> color_eyre::Result<()> {
    codec::self_test()?;
    println!(
        "bigiris self-test OK [sandbox + decode]  (bigiris={}  core={}  ai={} [onnx={}]  integrations={})",
        env!("CARGO_PKG_VERSION"),
        bigimage_core::version(),
        bigimage_ai::version(),
        bigimage_ai::onnx_available(),
        bigimage_integrations::version(),
    );
    Ok(())
}

fn run_convert(
    to: &str,
    opts: EncodeOptions,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = Format::parse(to)
        .map_err(|e| color_eyre::eyre::eyre!("formato de destino inválido: {e}"))?;
    tracing::info!(?target, ?opts, ?policy, count = files.len(), "convert");
    run_per_file(files, "convert", |f| bigimage_core::convert_file(f, target, &opts, policy))
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
    run_per_file(files, "resize", |f| {
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
    run_per_file(files, "flip", |f| bigimage_core::flip_file(f, axis, target, policy))
}

fn run_crop(
    rect: CropRect,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?rect, ?target, ?policy, count = files.len(), "crop");
    run_per_file(files, "crop", |f| bigimage_core::crop_file(f, rect, target, policy))
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
        .map_err(|e| color_eyre::eyre::eyre!("--bg inválido: {e}"))?;
    let opts = ExtentOptions { size, background, gravity };
    tracing::info!(?opts, ?target, ?policy, count = files.len(), "extent");
    run_per_file(files, "extent", |f| bigimage_core::extent_file(f, opts, target, policy))
}

fn run_adjust(
    ops: AdjustOps,
    to: Option<&str>,
    policy: OverwritePolicy,
    files: &[PathBuf],
) -> color_eyre::Result<()> {
    let target = parse_optional_target(to)?;
    tracing::info!(?ops, ?target, ?policy, count = files.len(), "adjust");
    run_per_file(files, "adjust", |f| bigimage_core::adjust_file(f, ops, target, policy))
}

fn parse_optional_target(to: Option<&str>) -> color_eyre::Result<Option<Format>> {
    match to {
        Some(s) => Ok(Some(
            Format::parse(s)
                .map_err(|e| color_eyre::eyre::eyre!("formato de destino inválido: {e}"))?,
        )),
        None => Ok(None),
    }
}

fn run_per_file<F>(files: &[PathBuf], op_name: &str, mut op: F) -> color_eyre::Result<()>
where
    F: FnMut(&PathBuf) -> bigimage_core::Result<ConvertOutcome>,
{
    let mut written = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for f in files {
        match op(f) {
            Ok(ConvertOutcome::Written { output }) => {
                written += 1;
                println!("ok  {} -> {}", f.display(), output.display());
            }
            Ok(ConvertOutcome::Skipped { output }) => {
                skipped += 1;
                println!("skip {} (já existe: {})", f.display(), output.display());
            }
            Err(e) => {
                failed += 1;
                eprintln!("err {}: {e}", f.display());
            }
        }
    }

    eprintln!("{op_name}: {written} gravado(s), {skipped} ignorado(s), {failed} falha(s)");

    if failed > 0 {
        std::process::exit(2);
    }
    Ok(())
}

fn run_remove_bg(model: &str, files: &[PathBuf]) -> color_eyre::Result<()> {
    tracing::info!(%model, count = files.len(), "remove-bg");
    if !bigimage_ai::onnx_available() {
        eprintln!(
            "aviso: esta build nao tem a feature `ai`. Recompile com \
             `cargo build --features ai` e instale onnxruntime (Arch: \
             `pacman -S onnxruntime`)."
        );
        std::process::exit(2);
    }
    if model != "birefnet-lite" {
        return Err(color_eyre::eyre::eyre!("modelo não suportado: {model}"));
    }

    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut session = bigimage_ai::background::BgSession::new(|_| {})?;
    for f in files {
        match remove_bg_one(f, &mut session) {
            Ok(out) => {
                ok += 1;
                println!("ok  {} -> {}", f.display(), out.display());
            }
            Err(e) => {
                fail += 1;
                eprintln!("err {}: {e}", f.display());
            }
        }
    }
    eprintln!("remove-bg: {ok} gravado(s), {fail} falha(s)");
    if fail > 0 {
        std::process::exit(2);
    }
    Ok(())
}

fn remove_bg_one(
    path: &std::path::Path,
    session: &mut bigimage_ai::background::BgSession,
) -> color_eyre::Result<PathBuf> {
    bigimage_core::content::ensure_static(path)?;
    let img =
        bigimage_core::open_image(path).map_err(|e| color_eyre::eyre::eyre!("decode: {e}"))?;
    let out_img = session.process(&img).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let dest = parent.join(format!("{stem}_nobg.png"));
    bigimage_core::save_image(
        out_img,
        &dest,
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )
    .map_err(|e| color_eyre::eyre::eyre!("encode: {e}"))?;
    Ok(dest)
}

/// Upscale via resize Lanczos3 (CPU). Produz saída `<stem>_<pct>pct.<ext>`.
/// O backend IA (Real-ESRGAN) entra aqui no futuro por trás de um
/// `--engine ai`, reusando a mesma API de saída.
fn run_upscale(factor: u8, policy: OverwritePolicy, files: &[PathBuf]) -> color_eyre::Result<()> {
    if !(2..=4).contains(&factor) {
        return Err(color_eyre::eyre::eyre!("fator inválido: {factor}. Use 2, 3 ou 4."));
    }
    let percent = f32::from(factor) * 100.0;
    let mode = ResizeMode::Percent(percent);
    tracing::info!(factor, ?policy, count = files.len(), "upscale (lanczos3)");
    let opts = bigimage_core::EncodeOptions::default();
    run_per_file(files, "upscale", |f| {
        bigimage_core::resize_file(f, mode, Filter::Lanczos3, None, &opts, policy)
    })
}

fn run_animate(
    output: &std::path::Path,
    delay_ms: u32,
    loop_count: u16,
    speed: i32,
    frames: &[PathBuf],
) -> color_eyre::Result<()> {
    let loop_mode = match loop_count {
        0 => LoopMode::Infinite,
        1 => LoopMode::Once,
        n => LoopMode::Finite(n),
    };
    let opts = AnimateOptions { delay_ms, loop_mode, speed };
    tracing::info!(?output, ?opts, frames = frames.len(), "animate");
    let written = bigimage_core::make_gif(frames, output, opts)
        .map_err(|e| color_eyre::eyre::eyre!("animate: {e}"))?;
    println!("ok  {} quadro(s) -> {}", frames.len(), written.display());
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchProfile {
    /// Target extension/name accepted by `Format::parse`.
    format: String,
    quality: Option<u8>,
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

fn run_batch(profile: &std::path::Path, inputs: &[PathBuf]) -> color_eyre::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    let text = std::fs::read_to_string(profile)
        .map_err(|e| color_eyre::eyre::eyre!("não foi possível ler {}: {e}", profile.display()))?;
    let profile: BatchProfile =
        toml::from_str(&text).map_err(|e| color_eyre::eyre::eyre!("perfil TOML inválido: {e}"))?;
    if profile.quality.is_some_and(|quality| !(1..=100).contains(&quality)) {
        return Err(color_eyre::eyre::eyre!("quality deve estar entre 1 e 100"));
    }
    let format = Format::parse(&profile.format)
        .map_err(|e| color_eyre::eyre::eyre!("formato do perfil inválido: {e}"))?;
    let policy = match profile.overwrite.trim().to_ascii_lowercase().as_str() {
        "skip" => OverwritePolicy::Skip,
        "replace" => OverwritePolicy::Replace,
        "increment" => OverwritePolicy::Increment,
        other => {
            return Err(color_eyre::eyre::eyre!(
                "overwrite inválido: {other:?}; use skip, replace ou increment"
            ));
        }
    };
    if let Some(dir) = &profile.output_dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            color_eyre::eyre::eyre!("não foi possível criar {}: {e}", dir.display())
        })?;
    }

    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let entries = std::fs::read_dir(input).map_err(|e| {
                color_eyre::eyre::eyre!("não foi possível ler {}: {e}", input.display())
            })?;
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
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(color_eyre::eyre::eyre!("nenhuma imagem encontrada para o lote"));
    }

    let total = files.len();
    let opts = EncodeOptions {
        quality: profile.quality,
        progressive: profile.progressive,
        optimize: profile.optimize,
        ..EncodeOptions::default()
    };
    let files = Arc::new(files);
    let output_dir = Arc::new(profile.output_dir);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let worker_count = bigimage_core::resources::batch_worker_count(&files);
    tracing::info!(?format, ?policy, total, worker_count, "batch");

    let workers: Vec<_> = (0..worker_count)
        .map(|_| {
            let files = files.clone();
            let output_dir = output_dir.clone();
            let next = next.clone();
            let tx = tx.clone();
            std::thread::spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= files.len() {
                    break;
                }
                let input = files[idx].clone();
                let result = bigimage_core::convert_file_to(
                    &input,
                    output_dir.as_deref(),
                    format,
                    &opts,
                    policy,
                )
                .map_err(|e| e.to_string());
                if tx.send((input, result)).is_err() {
                    break;
                }
            })
        })
        .collect();
    drop(tx);

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut first_error = None;
    for (done, (input, result)) in rx.into_iter().enumerate() {
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
                first_error.get_or_insert_with(|| format!("{}: {error}", input.display()));
                eprintln!("[{}/{}] erro {}: {error}", done + 1, total, input.display());
            }
        }
    }
    for worker in workers {
        if worker.join().is_err() {
            failed += 1;
            first_error.get_or_insert_with(|| "um worker terminou inesperadamente".to_string());
        }
    }
    println!("batch: {ok} gravado(s), {skipped} ignorado(s), {failed} falha(s)");
    if let Some(error) = first_error {
        return Err(color_eyre::eyre::eyre!(error));
    }
    Ok(())
}

/// Converte cada arquivo para PDF via `soffice --headless --convert-to pdf`.
/// Cobre ODT/DOCX/TXT/MD/RTF/ODS/XLSX/ODP/PPTX e outros formatos que o
/// LibreOffice aceita — ou seja, todo o conjunto que nosso irmão
/// BigOCRPDF **não** cobre (ele só faz imagens + PDFs existentes).
/// Saída vai ao lado de cada arquivo de origem.
fn run_to_pdf(files: &[PathBuf]) -> color_eyre::Result<()> {
    tracing::info!(count = files.len(), "to-pdf");
    // Verifica soffice uma única vez antes de iterar — falhar cedo se não
    // estiver instalado evita vomitar o mesmo erro N vezes.
    let soffice_path = which_soffice().ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "LibreOffice (`soffice`) não encontrado no PATH. \
             Instale `libreoffice-fresh` (recomendado) ou `libreoffice-still` \
             (ex.: `sudo pacman -S libreoffice-fresh`)."
        )
    })?;

    let mut ok = 0usize;
    let mut fail = 0usize;
    for file in files {
        let out_dir = file.parent().unwrap_or(std::path::Path::new("."));
        let status = std::process::Command::new(&soffice_path)
            .arg("--headless")
            .arg("--convert-to")
            .arg("pdf")
            .arg("--outdir")
            .arg(out_dir)
            .arg(file)
            .status();
        match status {
            Ok(s) if s.success() => {
                ok += 1;
                println!(
                    "ok  {} → {}.pdf",
                    file.display(),
                    file.file_stem().unwrap_or_default().to_string_lossy()
                );
            }
            Ok(s) => {
                fail += 1;
                eprintln!("err {}: soffice exit {}", file.display(), s);
            }
            Err(e) => {
                fail += 1;
                eprintln!("err {}: falha ao invocar soffice: {e}", file.display());
            }
        }
    }
    println!("to-pdf: {ok} gravado(s), {fail} falha(s)");
    if fail > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// Procura o binário `soffice` no PATH. Prefixo canonical do LibreOffice
/// em todas as distros. Retorna `None` se não existir.
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
            bigimage_integrations::install(scope)
                .map_err(|e| color_eyre::eyre::eyre!("install falhou: {e}"))?
        }
    };
    print_install_report(&report, "instalado");
    if report.is_success() {
        Ok(())
    } else {
        std::process::exit(2);
    }
}

fn run_uninstall() -> color_eyre::Result<()> {
    // Uninstall always runs against the user scope for now; a system-level
    // uninstall would require root and typically the distro package handles
    // it via `makepkg` / `.install` hooks.
    let scope = bigimage_integrations::Scope::User;
    tracing::info!(?scope, "uninstall-integrations");
    let report = bigimage_integrations::uninstall(scope)
        .map_err(|e| color_eyre::eyre::eyre!("uninstall falhou: {e}"))?;
    print_install_report(&report, "removido");
    if report.is_success() {
        Ok(())
    } else {
        std::process::exit(2);
    }
}

fn resolve_scope(user: bool, system: bool) -> bigimage_integrations::Scope {
    let _ = user; // User is the default; the flag exists for clarity.
    if system {
        bigimage_integrations::Scope::System
    } else {
        bigimage_integrations::Scope::User
    }
}

fn print_install_report(report: &bigimage_integrations::Report, verb: &str) {
    for outcome in &report.outcomes {
        let status = if outcome.error.is_some() {
            "FALHA"
        } else if outcome.files.is_empty() {
            if outcome.detected {
                "sem alterações"
            } else {
                "ignorado"
            }
        } else {
            verb
        };
        let detected = if outcome.detected { "detectado" } else { "ausente no PATH" };
        println!(
            "  {:<22}  [{}]  {:>2} arquivo(s)  ({})",
            outcome.fm.display_name(),
            status,
            outcome.files.len(),
            detected,
        );
        if let Some(err) = &outcome.error {
            eprintln!("    -> {err}");
        }
    }
    eprintln!(
        "{} total: {} arquivo(s) em {} gerenciador(es)",
        verb,
        report.files_touched(),
        report.outcomes.len()
    );
}

fn dispatch_gui(dialog: Option<Dialog>, files: &[PathBuf]) -> color_eyre::Result<()> {
    #[cfg(not(feature = "gui"))]
    {
        if let Some(d) = dialog {
            eprintln!(
                "aviso: build atual sem feature `gui`. Dialogo '{:?}' ignorado (recompile com --features gui).",
                d
            );
        } else if files.is_empty() {
            eprintln!(
                "aviso: build atual sem feature `gui`. Visualizador indisponivel (recompile com --features gui)."
            );
        } else {
            eprintln!(
                "aviso: build atual sem feature `gui`. Visualizador indisponivel; arquivos recebidos:"
            );
            for f in files {
                eprintln!("  - {}", f.display());
            }
        }
        Ok(())
    }

    #[cfg(feature = "gui")]
    {
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
