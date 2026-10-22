#[derive(shep_codec::Decode)]
struct Msg {
    a: u8,
    #[codec(default)]
    b: u8,
    c: u8,
}

fn main() {}
