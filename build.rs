fn main() {
    // WinFSP is only needed when a branch is mounted. Linking it as a normal import makes every
    // command fail to start on a machine without WinFSP installed, with a message about a missing DLL
    // that says nothing about mount. A delay load keeps `status` working and makes the dependency
    // appear only where it is used.
    //
    // The call is behind `cfg(windows)` rather than a runtime check: `winfsp` is declared under
    // `[target.'cfg(windows)'.build-dependencies]`, so on any other host the crate is not in scope and
    // the call would not compile even where it is never made.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();

    // The icon goes into the executable, so Explorer, the taskbar and the Start menu show the mark
    // rather than a blank generic file. A missing or malformed icon is a hard error here rather than
    // a silently blank one, because the failure would otherwise only be visible by looking at a file
    // listing on a Windows machine.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/gesta.ico");
        println!("cargo:rerun-if-changed=assets/gesta.rc");
        embed_resource::compile("assets/gesta.rc", embed_resource::NONE);
    }

    println!("cargo:rerun-if-changed=build.rs");
}
