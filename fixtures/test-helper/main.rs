// The repo's own small test helper binary (decision 0014): staged through
// the external-binaries pipeline in acceptance, where the installed app's
// PYLAIR_BINARIES map must resolve it. The pylair PR runs it end to end;
// this PR proves declaration, staging and resolution. With tectonic-shaped
// arguments it writes a tiny deterministic PDF, letting tests exercise the
// learning-path export pipeline without downloading the real tool.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(outdir_pos) = args.iter().position(|arg| arg == "--outdir") {
        if std::env::var_os("PYLAIR_TEST_HELPER_NETWORK_FAILURE").is_some() {
            eprintln!("error: failed to download \"latex/support.file\"; please check your network connection.");
            std::process::exit(42);
        }
        let outdir = args.get(outdir_pos + 1).expect("--outdir needs a path");
        let pdf = b"%PDF-1.1\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n";
        std::fs::write(std::path::Path::new(outdir).join("note.pdf"), pdf).expect("write note.pdf");
        println!("fixture tectonic ok");
    } else {
        println!("pylair-test-helper ok");
    }
}
