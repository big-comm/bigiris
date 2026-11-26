// SPDX-License-Identifier: GPL-3.0-or-later
//! Output planning shared by the CLI commands and the GUI batch runner:
//! which inputs write the same file, refused or run in order before any
//! work starts.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bigimage_core::{Format, OverwritePolicy};
use gettextrs::gettext;

use crate::codec::printable_path;
use crate::fill;

/// How a command names outputs: `<dir>/<stem><suffix>.<ext>`.
#[derive(Clone)]
pub(crate) struct Naming {
    /// `None` writes each output next to its input.
    pub(crate) output_dir: Option<PathBuf>,
    /// `None` keeps each input's own format.
    pub(crate) extension: Option<&'static str>,
    pub(crate) policy: OverwritePolicy,
    pub(crate) suffix: OutputSuffix,
}

/// What an output adds to its input's stem.
#[derive(Clone)]
pub(crate) enum OutputSuffix {
    /// Nothing: an output in the input's own format is the input itself.
    None,
    /// The same text for every input (`_rot90`, `_1920`), as the core's
    /// `filename_suffix` gives it.
    Fixed(String),
    /// Text that depends on each image and starts with this (`extent
    /// --aspect` names the output after its canvas): a shared stem is only a
    /// possible collision, not refused up front, and the runner reports an
    /// input that really replaced an earlier output.
    PerImage(&'static str),
}

impl Naming {
    /// Outputs next to their inputs, in `target` or each input's own format.
    pub(crate) fn image(
        target: Option<Format>,
        policy: OverwritePolicy,
        suffix: OutputSuffix,
    ) -> Naming {
        Naming {
            output_dir: None,
            extension: target.map(|format| format.extension()),
            policy,
            suffix,
        }
    }
}

/// Group inputs by destination, in input order, before any work starts.
/// Repeated spellings of one file (`a.png`, `./a.png`) are processed once;
/// a link to it in another folder writes its own output there and runs too.
/// Outputs whose names differ only in case are one file where their folder
/// folds case (vfat, exFAT, NTFS, casefolded folders).
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
/// actually wrote ([`bigimage_core::ConvertOutcome`]), never this planned one.
/// The error is the translated refusal naming each colliding group.
pub(crate) fn plan_outputs(
    files: &[PathBuf],
    naming: &Naming,
) -> Result<Vec<Vec<PathBuf>>, String> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut groups: Vec<Vec<PathBuf>> = Vec::new();
    let mut index = HashMap::new();
    let mut planned = Vec::new();
    for file in files {
        let identity = std::fs::canonicalize(file)
            .or_else(|_| std::path::absolute(file))
            .unwrap_or_else(|_| file.clone());
        let key = output_key(file, naming);
        if seen.insert((identity, key.clone())) {
            planned.push((key, file));
        }
    }
    merge_case_variants(&mut planned, folds_case);
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
    // Under skip, inputs writing over one of themselves all leave it as it
    // is (`x.png` and `x.webp` converted to WebP): nothing to refuse.
    let unsuffixed = matches!(naming.suffix, OutputSuffix::None);
    let collisions: Vec<_> = groups
        .iter()
        .filter(|group| group.len() > 1)
        .filter(|group| {
            naming.policy != OverwritePolicy::Skip
                || !unsuffixed
                || !group.iter().any(|file| writes_itself(file, naming))
        })
        .collect();
    if !collisions.is_empty()
        && naming.policy != OverwritePolicy::Increment
        && !matches!(naming.suffix, OutputSuffix::PerImage(_))
    {
        return Err(collision_refusal(&collisions));
    }
    // `a_rot90.png` replaced by the output of `a.png` while it is an input
    // of the same run, read before or after by another worker.
    if naming.policy == OverwritePolicy::Replace {
        let replaced = inputs_written_over(files, naming);
        if !replaced.is_empty() {
            return Err(replaced_input_refusal(&replaced));
        }
    }
    Ok(groups)
}

/// `(input, other)` pairs where the output of `input` under `naming` is,
/// or for a per-image suffix may be, `other`, another input of the run.
/// Outputs without a suffix that name another input share its output and
/// are refused as a collision already.
fn inputs_written_over<'a>(files: &'a [PathBuf], naming: &Naming) -> Vec<(&'a Path, &'a Path)> {
    let folders: Vec<PathBuf> = files
        .iter()
        .map(|file| {
            let dir = file.parent().filter(|dir| !dir.as_os_str().is_empty());
            let dir = dir.unwrap_or(Path::new("."));
            std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
        })
        .collect();
    let mut pairs = Vec::new();
    for file in files {
        let (dir, stem, extension) = output_key(file, naming);
        for (other, other_dir) in files.iter().zip(&folders) {
            let other_extension = other.extension().unwrap_or_default();
            if *other_dir != dir || other_extension != extension.as_os_str() {
                continue;
            }
            let other_stem = other.file_stem().unwrap_or_default().as_encoded_bytes();
            let mut output_stem = stem.clone();
            let names_other = match &naming.suffix {
                OutputSuffix::None => false,
                OutputSuffix::Fixed(suffix) => {
                    output_stem.push(suffix);
                    other_stem == output_stem.as_encoded_bytes()
                }
                OutputSuffix::PerImage(prefix) => {
                    output_stem.push(prefix);
                    other_stem.starts_with(output_stem.as_encoded_bytes())
                }
            };
            if names_other {
                pairs.push((file.as_path(), other.as_path()));
            }
        }
    }
    pairs
}

/// The refusal naming each input whose output would replace another input.
fn replaced_input_refusal(pairs: &[(&Path, &Path)]) -> String {
    let mut message = gettext("these outputs would replace other inputs of the same run:");
    for (input, other) in pairs {
        message.push_str("\n  ");
        message.push_str(&printable_path(input));
        message.push_str(" → ");
        message.push_str(&printable_path(other));
    }
    message.push('\n');
    message.push_str(&gettext("Process them separately or use the skip or increment policy."));
    message
}

/// How many inputs a run under `replace` would write over: a same-format
/// conversion next to its source is named like the source. Suffixed
/// outputs never are, and [`plan_outputs`] refuses those that would replace
/// another input.
#[cfg(feature = "gui")]
pub(crate) fn replaced_inputs(files: &[PathBuf], naming: &Naming) -> usize {
    use std::collections::HashSet;
    if naming.policy != OverwritePolicy::Replace || !matches!(naming.suffix, OutputSuffix::None) {
        return 0;
    }
    let inputs: HashSet<PathBuf> =
        files.iter().filter_map(|file| std::fs::canonicalize(file).ok()).collect();
    let replaced: HashSet<PathBuf> = files
        .iter()
        .filter_map(|file| {
            let (dir, stem, extension) = output_key(file, naming);
            let mut name = stem;
            name.push(".");
            name.push(extension);
            std::fs::canonicalize(dir.join(name)).ok()
        })
        .filter(|output| inputs.contains(output))
        .collect();
    replaced.len()
}

/// Whether the output of `file` under the unsuffixed `naming` is `file`. A
/// PNM output is `.pgm` or `.ppm` by its pixels, so any PNM spelling of the
/// input's own name may be it.
fn writes_itself(file: &Path, naming: &Naming) -> bool {
    let (dir, stem, extension) = output_key(file, naming);
    let is_pnm = |ext: &std::ffi::OsStr| {
        ext.to_str().is_some_and(|ext| Format::parse(ext).is_ok_and(|f| f == Format::Pnm))
    };
    let mut name = stem;
    name.push(".");
    match file.extension().filter(|own| is_pnm(&extension) && is_pnm(own)) {
        Some(own) => name.push(own),
        None => name.push(extension),
    }
    std::fs::canonicalize(dir.join(name))
        .is_ok_and(|output| std::fs::canonicalize(file).is_ok_and(|input| input == output))
}

/// The refusal naming each group of inputs that would write one file.
fn collision_refusal(collisions: &[&Vec<PathBuf>]) -> String {
    let mut message = gettext("several inputs would write the same output file:");
    for group in collisions {
        let names: Vec<_> = group.iter().map(|path| printable_path(path)).collect();
        message.push_str("\n  ");
        message.push_str(&names.join(", "));
    }
    message.push('\n');
    message.push_str(&gettext(
        "Rename the inputs, process them separately or use the increment policy.",
    ));
    message
}

type OutputKey = (PathBuf, OsString, OsString);

/// Give keys that differ only in ASCII case one spelling where their folder
/// treats such names as one file (`folds`), so they group as one output.
fn merge_case_variants<F: AsRef<Path>>(
    planned: &mut [(OutputKey, F)],
    folds: impl Fn(&Path) -> bool,
) {
    let folded = |key: &OutputKey| {
        let fold = |name: &OsString| name.as_encoded_bytes().to_ascii_lowercase();
        (key.0.clone(), fold(&key.1), fold(&key.2))
    };
    let mut spelling: HashMap<_, OutputKey> = HashMap::new();
    let mut folding: HashMap<PathBuf, bool> = HashMap::new();
    for (key, _) in planned.iter_mut() {
        let first = spelling.entry(folded(key)).or_insert_with(|| key.clone());
        if first != key && *folding.entry(key.0.clone()).or_insert_with(|| folds(&key.0)) {
            *key = first.clone();
        }
    }
}

/// Whether `dir` treats names that differ only in case as one file (vfat,
/// exFAT, NTFS, a casefolded folder), probed with a hidden temporary file.
/// A folder that cannot be probed counts as folding: the cost is a refusal.
fn folds_case(dir: &Path) -> bool {
    let prefix = format!("{}case-", bigimage_core::TEMP_FILE_PREFIX);
    let Ok(probe) = tempfile::Builder::new().prefix(&prefix).tempfile_in(dir) else {
        return true;
    };
    let name = probe.path().file_name().unwrap_or_default().to_ascii_uppercase();
    std::fs::symlink_metadata(dir.join(name)).is_ok()
}

/// Folder, stem and extension of the file `file` writes under `naming`.
fn output_key(file: &Path, naming: &Naming) -> OutputKey {
    let dir = naming
        .output_dir
        .as_deref()
        .or_else(|| file.parent())
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Without a target the core keeps the format found in the content,
    // so `a.jpg` holding a PNG writes `a_<suffix>.png`; plan by the same.
    let format = match naming.extension {
        Some(extension) => Format::parse(extension).ok(),
        None => bigimage_core::sniff_format(file),
    };
    let extension = match (format, naming.extension) {
        (Some(format), _) => format.output_extension(file).to_os_string(),
        (None, Some(extension)) => extension.into(),
        (None, None) => file.extension().unwrap_or_default().to_os_string(),
    };
    (dir, file.file_stem().unwrap_or_default().to_os_string(), extension)
}

/// `<dir>/<stem>_nobg.png` for a background removal of `input` (`.PNG` for
/// a `.PNG` input, as [`Format::output_extension`] names it), built from the
/// raw OS name so non-UTF-8 stems keep distinct outputs.
pub(crate) fn remove_bg_output(input: &Path) -> PathBuf {
    let mut name = input.file_stem().unwrap_or_default().to_os_string();
    name.push("_nobg.");
    name.push(Format::Png.output_extension(input));
    input.with_file_name(name)
}

/// Inputs of a batch, and the folder entries left out of them.
pub(crate) struct BatchFiles {
    /// Files named as inputs, and the supported images directly inside named
    /// folders.
    pub(crate) inputs: Vec<PathBuf>,
    /// `(entry, source)`: a folder image named like an output of `source`
    /// (`x.webp` or `x_1.webp` beside `x.png` in a WebP batch, `x_1.png` in
    /// a PNG one); converting it again would multiply outputs.
    pub(crate) earlier_outputs: Vec<(PathBuf, PathBuf)>,
}

/// The images directly inside `dir` whose extension names a supported
/// format, sorted; folders are never searched.
pub(crate) fn folder_images(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut images: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| Format::parse(ext).is_ok())
        })
        .collect();
    images.sort();
    Ok(images)
}

/// Files as given, folders as their [`folder_images`]; only under `increment`
/// does a folder receiving the outputs lose its earlier ones.
pub(crate) fn batch_files(
    inputs: &[PathBuf],
    format: Format,
    output_dir: Option<&Path>,
    policy: OverwritePolicy,
) -> Result<BatchFiles, String> {
    let mut files = BatchFiles { inputs: Vec::new(), earlier_outputs: Vec::new() };
    for input in inputs {
        if !input.is_dir() {
            files.inputs.push(input.clone());
            continue;
        }
        let images = folder_images(input).map_err(|error| {
            fill(
                &gettext("could not read {path}: {error}"),
                &[("path", &printable_path(input)), ("error", &error.to_string())],
            )
        })?;
        let writes_here = output_dir
            .is_none_or(|dir| std::fs::canonicalize(dir).ok() == std::fs::canonicalize(input).ok());
        if !writes_here || policy != OverwritePolicy::Increment {
            files.inputs.extend(images);
            continue;
        }
        let sources = earlier_output_sources(&images, format);
        for (index, image) in images.iter().enumerate() {
            // Two images naming each other (`a.pgm` and `a.ppm` in a PNM
            // batch): the first in sort order stays an input.
            let source =
                sources[index].filter(|&source| sources[source] != Some(index) || source < index);
            match source {
                Some(source) => files.earlier_outputs.push((image.clone(), images[source].clone())),
                None => files.inputs.push(image.clone()),
            }
        }
    }
    Ok(files)
}

/// For each of `images`, the other image whose output in `format` it is
/// named like, at its stem or a numbered one (`x` for `x_2_1`); never itself.
fn earlier_output_sources(images: &[PathBuf], format: Format) -> Vec<Option<usize>> {
    let mut by_stem: HashMap<OsString, Vec<usize>> = HashMap::new();
    for (index, image) in images.iter().enumerate() {
        by_stem
            .entry(image.file_stem().unwrap_or_default().to_os_string())
            .or_default()
            .push(index);
    }
    let source_of = |index: usize| {
        let image = &images[index];
        let named_like_output_of = |source: &Path| match format {
            // A PNM output is `.pgm` or `.ppm` by its pixels, unknown before decoding.
            Format::Pnm => image
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| Format::parse(ext).is_ok_and(|parsed| parsed == Format::Pnm)),
            _ => image.extension() == Some(format.output_extension(source)),
        };
        let mut stem = Some(image.file_stem().unwrap_or_default().to_os_string());
        let mut numbered = false;
        while let Some(current) = stem {
            let candidates = by_stem.get(&current).into_iter().flatten().copied();
            let mut found = candidates.filter(|&source| numbered || source != index);
            if let Some(source) = found.find(|&source| named_like_output_of(&images[source])) {
                return Some(source);
            }
            stem = numbered_base(&current);
            numbered = true;
        }
        None
    };
    (0..images.len()).map(source_of).collect()
}

/// `x` for a stem `x_N` as written by the `increment` policy (N >= 1, no
/// leading zero).
pub(crate) fn numbered_base(stem: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_groups_numbered_stems() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["x_1.png", "IMG_2.png", "x.png", "x_1_1.png", "x_01.png", "_1.png"];
        let files: Vec<_> = names.iter().map(|name| dir.path().join(name)).collect();
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }
        let naming = Naming::image(
            Some(Format::Png),
            OverwritePolicy::Increment,
            OutputSuffix::Fixed("_x".into()),
        );
        let groups = plan_outputs(&files, &naming).unwrap();
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
        let naming = Naming::image(
            Some(Format::Png),
            OverwritePolicy::Skip,
            OutputSuffix::Fixed("_x".into()),
        );
        assert_eq!(plan_outputs(&files, &naming).unwrap().len(), names.len());
    }

    /// Mirrors the core's naming: a same-format output keeps the input's
    /// extension, so `photo.PNG` and `photo.png` write different files while
    /// `photo.jpeg` and `photo.jpg` converted to PNG both write `photo.png`.
    #[test]
    fn same_format_keeps_input_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name);
        let png = |names: &[&str]| {
            let files: Vec<_> = names.iter().map(|name| file(name)).collect();
            plan_outputs(
                &files,
                &Naming::image(
                    Some(Format::Png),
                    OverwritePolicy::Skip,
                    OutputSuffix::Fixed("_x".into()),
                ),
            )
        };
        assert_eq!(png(&["photo.PNG", "photo.png"]).unwrap().len(), 2);
        assert!(png(&["photo.jpeg", "photo.jpg"]).is_err());
        assert!(png(&["noext", "noext.png"]).is_err(), "an extensionless PNG writes noext.png");
    }

    /// Only `replace` writes over an input, and only one named like its
    /// output: `b.jpg` to JPEG next to itself, never `b.jpg` to PNG or a
    /// run into another folder.
    #[cfg(feature = "gui")]
    #[test]
    fn counts_inputs_a_replace_would_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["b.jpg", "c.JPEG", "d.png"].map(|name| dir.path().join(name)).into();
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }
        let naming = |format, policy| Naming::image(Some(format), policy, OutputSuffix::None);
        assert_eq!(replaced_inputs(&files, &naming(Format::Jpeg, OverwritePolicy::Replace)), 2);
        assert_eq!(replaced_inputs(&files, &naming(Format::Png, OverwritePolicy::Replace)), 1);
        assert_eq!(replaced_inputs(&files, &naming(Format::WebP, OverwritePolicy::Replace)), 0);
        assert_eq!(replaced_inputs(&files, &naming(Format::Jpeg, OverwritePolicy::Skip)), 0);
        let elsewhere = tempfile::tempdir().unwrap();
        let other_folder = Naming {
            output_dir: Some(elsewhere.path().to_path_buf()),
            ..naming(Format::Jpeg, OverwritePolicy::Replace)
        };
        assert_eq!(replaced_inputs(&files, &other_folder), 0);
    }

    /// Under replace, a suffixed output may not name another input: it
    /// would be replaced while another worker reads it. Skip and increment
    /// leave it alone, and an aspect letterbox refuses any name its canvas
    /// could take.
    #[test]
    fn replace_refuses_writing_another_input() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["a.png", "a_rot90.png", "b.png", "b_extent-9x9.png"]
            .map(|name| dir.path().join(name))
            .into();
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }
        let rotate = |policy| Naming::image(None, policy, OutputSuffix::Fixed("_rot90".into()));
        let refusal = plan_outputs(&files, &rotate(OverwritePolicy::Replace)).unwrap_err();
        assert!(refusal.contains("a.png") && refusal.contains("a_rot90.png"), "{refusal}");
        assert!(!refusal.contains("b.png"), "{refusal}");
        for policy in [OverwritePolicy::Skip, OverwritePolicy::Increment] {
            assert!(plan_outputs(&files, &rotate(policy)).is_ok(), "{policy:?}");
        }
        let flip =
            Naming::image(None, OverwritePolicy::Replace, OutputSuffix::Fixed("_flipH".into()));
        assert!(plan_outputs(&files, &flip).is_ok());
        let letterbox = Naming::image(
            Some(Format::Png),
            OverwritePolicy::Replace,
            OutputSuffix::PerImage(bigimage_core::extent::EXTENT_SUFFIX),
        );
        let refusal = plan_outputs(&files, &letterbox).unwrap_err();
        assert!(refusal.contains("b_extent-9x9.png") && !refusal.contains("a.png"), "{refusal}");
    }

    const EARLIER_NAMES: [&str; 10] = [
        "x.png",
        "x_1.png",
        "x_1_1.png",
        "y.jpg",
        "y.png",
        "g.png",
        "g.pgm",
        "Z.PNG",
        "a.pgm",
        "a.ppm",
    ];

    /// A folder of [`EARLIER_NAMES`] scanned by [`batch_files`]: its input
    /// names and its `(earlier output, source)` names.
    fn scan_earlier(
        format: Format,
        policy: OverwritePolicy,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let dir = tempfile::tempdir().unwrap();
        for name in EARLIER_NAMES {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let files = batch_files(&[dir.path().to_path_buf()], format, None, policy).unwrap();
        let name = |path: &PathBuf| path.file_name().unwrap().to_str().unwrap().to_owned();
        let earlier = files.earlier_outputs.iter().map(|(a, b)| (name(a), name(b))).collect();
        (files.inputs.iter().map(name).collect(), earlier)
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }

    /// Under increment, earlier outputs are matched by the name the source's
    /// output takes: a same-format source keeps its name, so only its
    /// numbered copies are outputs, and a PNM output may be `.pgm` or `.ppm`.
    #[test]
    fn earlier_outputs_follow_output_names() {
        let (inputs, earlier) = scan_earlier(Format::Png, OverwritePolicy::Increment);
        assert_eq!(inputs, ["Z.PNG", "a.pgm", "a.ppm", "g.pgm", "x.png", "y.jpg"]);
        let expected = [
            ("g.png", "g.pgm"),
            ("x_1.png", "x.png"),
            ("x_1_1.png", "x_1.png"),
            ("y.png", "y.jpg"),
        ];
        assert_eq!(earlier, pairs(&expected));
        // Two PNM images naming each other: the first in sort order stays.
        let (inputs, earlier) = scan_earlier(Format::Pnm, OverwritePolicy::Increment);
        assert_eq!(earlier, pairs(&[("a.ppm", "a.pgm"), ("g.pgm", "g.png")]));
        assert_eq!(inputs.len(), EARLIER_NAMES.len() - 2);
    }

    /// Only `increment` writes numbered names: under skip and replace, or
    /// with the outputs in another folder, every image is an input.
    #[test]
    fn other_policies_keep_every_image() {
        for policy in [OverwritePolicy::Skip, OverwritePolicy::Replace] {
            let (inputs, earlier) = scan_earlier(Format::Png, policy);
            assert!(earlier.is_empty(), "{policy:?}: {earlier:?}");
            assert_eq!(inputs.len(), EARLIER_NAMES.len());
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.png"), b"").unwrap();
        std::fs::write(dir.path().join("x_1.png"), b"").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let folder = [dir.path().to_path_buf()];
        let files =
            batch_files(&folder, Format::Png, Some(elsewhere.path()), OverwritePolicy::Increment);
        assert_eq!(files.unwrap().inputs.len(), 2);
    }
}
