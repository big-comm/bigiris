// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline translations for installed file-manager actions.
mod data {
    include!("i18n_data.rs");
}

pub(crate) const fn mark(text: &'static str) -> &'static str {
    text
}

pub(crate) fn lookup<'a>(language: &str, source: &'a str) -> &'a str {
    data::CATALOGS
        .iter()
        .find(|(lang, _)| *lang == language)
        .and_then(|(_, entries)| entries.iter().find(|(key, _)| *key == source))
        .map_or(source, |(_, translated)| *translated)
}

pub(crate) fn localized(key: &str, render: impl Fn(&str) -> String) -> String {
    data::CATALOGS
        .iter()
        .map(|(lang, _)| {
            let value =
                render(lang).replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r");
            format!("{key}[{lang}]={value}\n")
        })
        .collect()
}

pub(crate) fn localized_name(key: &str, source: &str) -> String {
    localized(key, |lang| lookup(lang, source).to_owned())
}

pub(crate) fn flat_label(lang: &str, action: &crate::Action) -> String {
    match action.submenu {
        Some(sub) => {
            format!("BigIris ▸ {} ▸ {}", lookup(lang, sub.label), lookup(lang, action.label))
        }
        None => format!("{} (BigIris)", lookup(lang, action.label)),
    }
}

pub(crate) fn xml_labels(tag: &str, source: &str) -> String {
    data::CATALOGS
        .iter()
        .map(|(lang, _)| {
            format!(
                "\t<{tag} xml:lang=\"{lang}\">{}</{tag}>\n",
                crate::thunar::escape_xml(lookup(lang, source))
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_locale_falls_back_without_changing_commands() {
        assert_eq!(lookup("xx", "Converter"), "Converter");
        assert_eq!(lookup("en", "unknown"), "unknown");
    }
    #[test]
    fn desktop_values_escape_line_breaks() {
        let text = localized("Name", |_| "one\ntwo\\three".to_owned());
        assert_eq!(text.lines().count(), 29);
        assert!(text.contains("Name[en]=one\\ntwo\\\\three\n"));
    }
    #[test]
    fn known_locales_have_translated_folder_actions() {
        assert_eq!(lookup("en", "Processar esta pasta…"), "Process this folder…");
        assert_ne!(lookup("de", "Processar esta pasta…"), "Processar esta pasta…");
    }
}
