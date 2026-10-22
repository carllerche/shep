#[derive(shep_codec::Encode)]
enum Op {
    #[codec(tag = 256)]
    Put,
}

fn main() {}
