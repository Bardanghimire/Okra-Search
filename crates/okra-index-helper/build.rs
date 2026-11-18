fn main() {
    // Opt in to the UAC manifest explicitly so the ordinary test harness never prompts.
    let is_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let admin_manifest = std::env::var_os("CARGO_FEATURE_ADMIN_MANIFEST").is_some();
    if is_windows && admin_manifest {
        let mut resources = winres::WindowsResource::new();
        resources
            .set("FileDescription", "Okra Brown Search NTFS Index Helper")
            .set("ProductName", "Okra Brown Search")
            .set_manifest(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
            );
        if let Err(error) = resources.compile() {
            panic!("Could not embed the administrator manifest: {error}");
        }
    }
}
