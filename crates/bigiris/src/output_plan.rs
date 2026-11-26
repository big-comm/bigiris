// SPDX-License-Identifier: GPL-3.0-or-later
//! Output planning shared by the CLI commands and the GUI batch runner:
//! which inputs write the same file, refused or run in order before any
//! work starts.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bigimage_core::{Format, OverwritePolicy};
use gettextrs::gettext;

use crate::codec::printable_path;

/// How a command names outputs: `<dir>/<stem><suffix>.<ext>`. The suffix is
/// usually fixed per invocation, so inputs sharing directory, stem and
/// extension resolve to the same file.
#[derive(Clone)]
pub(crate) struct Naming {
    /// `None` writes each output next to its input.
    pub(crate) output_dir: Option<PathBuf>,
    /// `None` keeps each input's own format.
    pub(crate) extension: Option<&'static str>,
    pub(crate) policy: OverwritePolicy,
    /// The suffix depends on each image (`extent --aspect` names the output
    /// after the resulting canvas), so a shared stem is only a possible
    /// collision: it is not refused up front, and the runner reports an
    /// input that really replaced an earlier output.
    pub(crate) per_image_suffix: bool,
}

impl Naming {
    /// Outputs next to their inputs, in `target` or each input's own format.
    pub(crate) fn image(target: Option<Format>, policy: OverwritePolicy) -> Naming {
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
/// actually wrote ([`bigimage_core::ConvertOutcome`]), never this planned one.
/// The error is the translated refusal naming each colliding group.
pub(crate) fn plan_outputs(
    files: &[PathBuf],
    naming: &Naming,
) -> Result<Vec<Vec<PathBuf>>, String> {
    use std::collections::{HashMap, HashSet};
    let mut seen = HashSet::new();
    let mut groups: Vec<Vec<PathBuf>> = Vec::new();
    let mut index = HashMap::new();
    let mut planned = Vec::new();
    for file in files {
        let identity = std::fs::canonicalize(file)
            .or_else(|_| std::path::absolute(file))
            .unwrap_or_else(|_| file.clone());
        if seen.insert(identity) {
            planned.push((output_key(file, naming), file));
        }
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
            let names: Vec<_> = group.iter().map(|path| printable_path(path)).collect();
            message.push_str("\n  ");
            message.push_str(&names.join(", "));
        }
        message.push('\n');
        message.push_str(&gettext(
            "Rename the inputs, process them separately or use the increment policy.",
        ));
        return Err(message);
    }
    Ok(groups)
}

/// Folder, stem and extension of the file `file` writes under `naming`.
fn output_key(file: &Path, naming: &Naming) -> (PathBuf, OsString, OsString) {
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
        let naming = Naming::image(Some(Format::Png), OverwritePolicy::Increment);
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
        let naming = Naming::image(Some(Format::Png), OverwritePolicy::Skip);
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
            plan_outputs(&files, &Naming::image(Some(Format::Png), OverwritePolicy::Skip))
        };
        assert_eq!(png(&["photo.PNG", "photo.png"]).unwrap().len(), 2);
        assert!(png(&["photo.jpeg", "photo.jpg"]).is_err());
        assert!(png(&["noext", "noext.png"]).is_err(), "an extensionless PNG writes noext.png");
    }
}
