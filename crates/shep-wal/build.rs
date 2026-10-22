fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(
        ["shep/wal/raft/v1/raft.proto"],
        ["proto", "../shep-wal-api/proto"],
    )?;
    tonic_prost_build::configure()
        .bytes(".")
        .build_client(true)
        .build_server(true)
        .extern_path(".shep.wal.v1", "::shep_wal_api::proto")
        .compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=../shep-wal-api/proto");
    Ok(())
}
