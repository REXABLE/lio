use snarkos_models::parameters::Parameter;

pub struct GenesisMemo;

impl Parameter for GenesisMemo {
    const CHECKSUM: &'static str = "";
    const SIZE: u64 = 64;

    fn load_bytes() -> Vec<u8> {
        let buffer = include_bytes!("genesis_memo");
        buffer.to_vec()
    }
}
