fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the protos in-process, so building needs only Cargo.
    let fds = protox::compile(["shep/wal/v1/wal.proto"], ["proto"])?;
    tonic_prost_build::configure().bytes(".").compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
