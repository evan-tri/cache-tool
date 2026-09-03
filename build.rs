fn main() {
    let fds = protox::compile(["proto/cache.proto"], ["."]).unwrap();

    tonic_build::configure().compile_fds(fds).unwrap();

    println!("cargo:rerun-if-changed=proto/cache.proto");
}
