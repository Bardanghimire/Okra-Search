fn main() {
    println!("cargo:rerun-if-changed=../../assets/okra-brown-search.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").map_or(false, |os| os == "windows") {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("../../assets/okra-brown-search.ico");
        resource.set("FileDescription", "Okra Brown Search");
        resource.set("ProductName", "Okra Brown Search");
        if let Err(error) = resource.compile() {
            panic!("Could not embed the Okra Brown Search application icon: {error}");
        }
    }
}
