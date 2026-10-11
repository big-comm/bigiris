Name:           bigiris
Version:        0.1.0
Release:        1
Summary:        Fast image viewer and batch converter for Linux
License:        GPL-3.0-or-later
URL:            https://github.com/xathay/bigiris
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz
# Generated with packaging/openmandriva/make-vendor-tarball.sh. Keeping
# crates vendored makes ABF builds reproducible and independent of crates.io.
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo >= 1.92
BuildRequires:  rust >= 1.92
BuildRequires:  gettext
BuildRequires:  desktop-file-utils
BuildRequires:  appstream
BuildRequires:  pkgconfig(gtk4) >= 4.12
BuildRequires:  pkgconfig(libadwaita-1) >= 1.5
BuildRequires:  pkgconfig(dav1d)
BuildRequires:  pkgconfig(openssl)
BuildRequires:  onnxruntime-devel
Requires:       bubblewrap
Requires:       util-linux
Requires:       gtk4.0
Requires:       lib64adwaita
Requires:       onnxruntime
Requires:       hicolor-icon-theme

%description
BigIris combines a compact, responsive image viewer with Prisma, an optional
workspace for conversions, resize, crop, colour adjustment, animation and
parallel folder processing. It includes native Dolphin ServiceMenus and a CLI.

%prep
%autosetup -p1 -a1

%build
export CARGO_HOME="%{_builddir}/cargo-home"
export CARGO_TARGET_DIR="%{_builddir}/target"
export CARGO_NET_OFFLINE=true
export RUSTFLAGS="-C debuginfo=2"
cargo build --locked --offline --release --features "gui ai" --package bigiris

while read -r lang; do
    [ -z "$lang" ] && continue
    case "$lang" in \#*) continue ;; esac
    msgfmt -o "data/po/${lang}.mo" "data/po/${lang}.po"
done < data/po/LINGUAS

%check
export CARGO_HOME="%{_builddir}/cargo-home"
export CARGO_TARGET_DIR="%{_builddir}/target"
export CARGO_NET_OFFLINE=true
cargo test --locked --offline --release --workspace --all-features
"%{_builddir}/target/release/bigiris" --self-test
desktop-file-validate data/com.biglinux.Iris.desktop
appstreamcli validate --no-net data/com.biglinux.Iris.metainfo.xml
glib-compile-schemas --strict --dry-run data

%install
install -Dm755 "%{_builddir}/target/release/bigiris" \
    "%{buildroot}%{_bindir}/bigiris"
install -Dm644 data/com.biglinux.Iris.desktop \
    "%{buildroot}%{_datadir}/applications/com.biglinux.Iris.desktop"
install -Dm644 data/com.biglinux.Iris.metainfo.xml \
    "%{buildroot}%{_datadir}/metainfo/com.biglinux.Iris.metainfo.xml"
install -Dm644 data/com.biglinux.Iris.gschema.xml \
    "%{buildroot}%{_datadir}/glib-2.0/schemas/com.biglinux.Iris.gschema.xml"
install -Dm644 data/icons/hicolor/scalable/apps/com.biglinux.Iris.svg \
    "%{buildroot}%{_datadir}/icons/hicolor/scalable/apps/com.biglinux.Iris.svg"
while read -r lang; do
    [ -z "$lang" ] && continue
    case "$lang" in \#*) continue ;; esac
    install -Dm644 "data/po/${lang}.mo" \
        "%{buildroot}%{_datadir}/locale/${lang}/LC_MESSAGES/bigiris.mo"
done < data/po/LINGUAS

"%{_builddir}/target/release/bigiris" install-integrations --system \
    --destdir="%{buildroot}"

%files
%license LICENSE
%doc README.md
%{_bindir}/bigiris
%{_datadir}/applications/com.biglinux.Iris.desktop
%{_datadir}/metainfo/com.biglinux.Iris.metainfo.xml
%{_datadir}/glib-2.0/schemas/com.biglinux.Iris.gschema.xml
%{_datadir}/icons/hicolor/scalable/apps/com.biglinux.Iris.svg
%{_datadir}/locale/*/LC_MESSAGES/bigiris.mo
%{_datadir}/kio/servicemenus/bigiris-*.desktop
%{_datadir}/nemo/actions/bigiris-*.nemo_action
%{_datadir}/file-manager/actions/bigiris-*.desktop
%{_datadir}/contractor/bigiris-*.contract
%{_datadir}/nautilus-python/extensions/bigiris-menu.py
%config(noreplace) %{_sysconfdir}/xdg/Thunar/uca.xml

%changelog
* Sat Sep 12 2026 Leonardo Athayde <leoathayde@gmail.com> 0.1.0-1
- Initial OpenMandriva package
