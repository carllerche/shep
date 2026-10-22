#[derive(shep_codec::Encode)]
#[codec(transparent, version = 1)]
struct Msg(u8);

fn main() {}
