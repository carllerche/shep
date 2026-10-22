#[derive(shep_codec::Encode)]
struct Msg {
    #[codec(skip)]
    a: u8,
}

fn main() {}
