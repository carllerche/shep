#[derive(shep_codec::Encode)]
enum Op {
    Put,
    #[codec(tag = 0)]
    Delete,
}

fn main() {}
