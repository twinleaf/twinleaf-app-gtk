// Compile the bundled symbolic icons into a GResource embedded in the binary.
fn main() {
    glib_build_tools::compile_resources(
        &["resources"],
        "resources/twinleaf.gresource.xml",
        "twinleaf.gresource",
    );
}
