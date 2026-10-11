# OpenMandriva / ABF packaging

The spec builds the full GTK/AI application offline. Before submitting a new
version to ABF:

1. Create and push the matching Git tag, for example `v0.1.0`.
2. Run `bash packaging/openmandriva/make-vendor-tarball.sh` with the same Rust
   toolchain used for `Cargo.lock`.
3. Upload the generated `bigiris-<version>-vendor.tar.xz` as `Source1` in ABF.
4. Add the GitHub tag archive as `Source0`, then build the spec for Cooker.

The vendor archive contains only third-party Cargo sources plus Cargo's source
replacement configuration. It should not be committed to Git.

For local validation, use `rpmbuild -ba packaging/openmandriva/bigiris.spec`
with both source archives in `%{_sourcedir}`. The `%check` section runs the
workspace tests, CLI self-test, desktop entry, AppStream and GSettings checks.
