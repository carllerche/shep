#![forbid(unsafe_code)]
use shep_smp2::{Buffer, Config, Runtime, sleep, spawn};
use std::{io, time::Duration};
fn main() -> io::Result<()> {
    let mut runtime = Runtime::new(Config::default())?;
    // Metadata opens stay outside the reactor until a bounded helper pool is added.
    let file = runtime.io().file(tempfile::tempfile()?);
    runtime.block_on(async move {
        let file = spawn(async move {
            let (result, _) = file
                .write_at(Buffer::from_slice(b"native io_uring", 4096)?, 0)
                .await;
            result?;
            file.sync_data().await?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(io::Error::other)??;
        sleep(Duration::from_millis(1)).await;
        let (result, buffer) = file.read_at(Buffer::new(4096, 4096)?, 0).await;
        let bytes = result?;
        println!("{}", String::from_utf8_lossy(&buffer[..bytes]));
        Ok::<_, io::Error>(())
    })??;
    runtime.shutdown()
}
