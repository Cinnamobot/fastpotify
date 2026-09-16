//! Compiles translations, embeds Windows resources and links GLEW for libprojectM.

#[path = "build_support/catalogs.rs"]
mod catalogs;

/// Known names for vcpkg's static GLEW library, in preferred order.
/// Names differ across vcpkg versions and triplets, so use the installed one.
#[cfg(windows)]
const GLEW_NAMES: &[&str] = &["glew32s", "libglew32", "glew32"];

/// Returns the vcpkg triplet matching the target architecture and CRT mode.
#[cfg(windows)]
fn vcpkg_triplet() -> Option<String> {
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        _ => return None,
    };
    let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|feature| feature == "crt-static");
    let suffix = if static_crt { "static" } else { "static-md" };
    Some(format!("{arch}-windows-{suffix}"))
}

/// Returns the known GLEW library installed in `lib`.
#[cfg(windows)]
fn glew_library(lib: &std::path::Path) -> Option<&'static str> {
    let present: Vec<String> = std::fs::read_dir(lib)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.eq_ignore_ascii_case("lib") {
                Some(path.file_stem()?.to_str()?.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();
    GLEW_NAMES
        .iter()
        .copied()
        .find(|name| present.iter().any(|found| found == name))
}

fn main() {
    println!("cargo:rerun-if-changed=build_support/catalogs.rs");
    println!("cargo:rerun-if-changed=assets/i18n");
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    let mut modules = Vec::new();
    for entry in std::fs::read_dir("assets/i18n").expect("translation directory") {
        let path = entry.expect("translation entry").path();
        if path.extension().is_some_and(|ext| ext == "po") {
            let filename = path.file_stem().expect("catalog filename");
            let module = filename
                .to_str()
                .expect("catalog identifier")
                .replace('-', "_")
                .to_ascii_lowercase();
            assert!(
                module
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "invalid catalog identifier"
            );
            let generated = output.join(filename).with_extension("rs");
            catalogs::compile(&path, &generated)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            modules.push(format!("#[path = {generated:?}]\nmod {module};\n"));
        }
    }
    println!("cargo:rerun-if-changed=tests/fixtures/translation.po");
    catalogs::compile(
        std::path::Path::new("tests/fixtures/translation.po"),
        &output.join("test_translation.rs"),
    )
    .expect("translation regression fixture");
    // Generated files are modules so they retain their own module attributes.
    // Sort directory entries for reproducible builds.
    modules.sort();
    std::fs::write(output.join("catalogs.rs"), modules.concat()).expect("catalog module index");
    std::fs::write(
        output.join("test_catalog.rs"),
        format!(
            "#[path = {:?}]\nmod fixture;\n",
            output.join("test_translation.rs")
        ),
    )
    .expect("test catalog module index");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/windows/fastpotify.ico");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("packaging/windows/fastpotify.ico")
            .set("ProductName", "Spotifast")
            .set("FileDescription", "A native Spotify client");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
        // Static libprojectM requires the GLEW library installed by vcpkg.
        if std::env::var_os("CARGO_FEATURE_MILKDROP").is_some() {
            println!("cargo:rerun-if-env-changed=VCPKG_INSTALLATION_ROOT");
            if let (Some(root), Some(triplet)) =
                (std::env::var_os("VCPKG_INSTALLATION_ROOT"), vcpkg_triplet())
            {
                let lib = std::path::Path::new(&root)
                    .join("installed")
                    .join(triplet)
                    .join("lib");
                println!("cargo:rustc-link-search=native={}", lib.display());
                match glew_library(&lib) {
                    Some(name) => println!("cargo:rustc-link-lib=static={name}"),
                    None => {
                        // Include the directory listing in the error for diagnosis.
                        let listing = std::fs::read_dir(&lib)
                            .map(|entries| {
                                entries
                                    .flatten()
                                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_else(|error| format!("unreadable: {error}"));
                        println!(
                            "cargo:warning=no GLEW library in {}; it holds: {listing}",
                            lib.display()
                        );
                    }
                }
            }
        }
    }
}
