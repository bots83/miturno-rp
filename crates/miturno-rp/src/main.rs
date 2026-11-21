//! miturno-rp: cliente de Remote Play de MiTurno (ver README).

fn main() {
    println!("miturno-rp {}", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn el_binario_compila() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
