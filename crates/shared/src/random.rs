pub fn bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("operating system entropy");
    bytes
}

pub fn u64() -> u64 {
    u64::from_le_bytes(bytes())
}

pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", hex::encode(bytes::<12>()))
}

pub fn jittered(cap: std::time::Duration) -> std::time::Duration {
    cap.mul_f64(u64() as f64 / u64::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_unique() {
        let first = id("ord");
        assert!(first.starts_with("ord_") && first.len() == 28);
        assert_ne!(first, id("ord"));
    }
}
