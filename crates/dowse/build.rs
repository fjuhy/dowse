fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=windows/indexray.rc");
        println!("cargo:rerun-if-changed=windows/indexray.manifest");
        println!("cargo:rerun-if-changed=../dowse-app/src-tauri/icons/icon.ico");
        embed_resource::compile_for(
            "windows/indexray.rc",
            ["IndexRay"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("failed to embed IndexRay Windows resources");
    }
}
