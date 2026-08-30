fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    let info = codescope_testutil::go_fixture::build_fixture(std::path::Path::new(&dir)).expect("build fixture");
    println!("{}", info.root.display());
}
