#[derive(shep_codec::Encode)]
#[codec(version = 1, compat = 2)]
struct Msg {
    a: u8,
}

fn main() {}
