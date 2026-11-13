//! Icône de l'application et informations de version (VERSIONINFO) intégrées à l'exécutable.
//! La version vient de `[workspace.package]` (CARGO_PKG_VERSION) : une seule source de vérité.

fn main() {
    println!("cargo:rerun-if-changed=../../installer/grx.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../installer/grx.ico")
        .set("ProductName", "GamingRemoteExperience")
        .set("FileDescription", "GamingRemoteExperience - interface")
        .set("CompanyName", "GamingRemoteExperience")
        .set("LegalCopyright", "Licence MIT")
        .set("OriginalFilename", "grx-gui.exe")
        .set("InternalName", "grx-gui");
    if let Err(e) = res.compile() {
        // Pas de SDK Windows (rc.exe) : l'exécutable reste fonctionnel, sans icône.
        println!("cargo:warning=ressources Windows non intégrées : {e}");
    }
}
