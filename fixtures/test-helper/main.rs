// The repo's own small test helper binary (decision 0014): staged through
// the external-binaries pipeline in acceptance, where the installed app's
// PYLAIR_BINARIES map must resolve it. The pylair PR runs it end to end;
// this PR proves declaration, staging and resolution.
fn main() {
    println!("pylair-test-helper ok");
}
