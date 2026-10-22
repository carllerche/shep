#[derive(shep_codec::Encode)]
#[codec(transparent)]
enum Msg {
    A(u8),
}

fn main() {}
