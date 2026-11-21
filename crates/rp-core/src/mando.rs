//! Estado del mando PS5 (DualSense) tal como viaja entre la web y la sesión.
//! Los bits son fijos y los comparte la web (`packages/controls`).

pub const CRUZ: u32 = 1 << 0;
pub const CIRCULO: u32 = 1 << 1;
pub const CUADRADO: u32 = 1 << 2;
pub const TRIANGULO: u32 = 1 << 3;
pub const L1: u32 = 1 << 4;
pub const R1: u32 = 1 << 5;
pub const L3: u32 = 1 << 6;
pub const R3: u32 = 1 << 7;
pub const OPTIONS: u32 = 1 << 8;
pub const CREATE: u32 = 1 << 9;
pub const PS: u32 = 1 << 10;
pub const PANEL: u32 = 1 << 11;
pub const ARRIBA: u32 = 1 << 12;
pub const ABAJO: u32 = 1 << 13;
pub const IZQUIERDA: u32 = 1 << 14;
pub const DERECHA: u32 = 1 << 15;

/// Foto completa del mando. Sticks de -32768 a 32767 (como libchiaki), gatillos de 0 a 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EstadoMando {
    pub botones: u32,
    pub l2: u8,
    pub r2: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

impl EstadoMando {
    /// Todo suelto: sin botones, gatillos ni sticks.
    pub fn suelto() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn los_bits_son_los_acordados() {
        assert_eq!(CRUZ, 1);
        assert_eq!(CIRCULO, 2);
        assert_eq!(CUADRADO, 4);
        assert_eq!(TRIANGULO, 8);
        assert_eq!(L1, 16);
        assert_eq!(R1, 32);
        assert_eq!(L3, 64);
        assert_eq!(R3, 128);
        assert_eq!(OPTIONS, 256);
        assert_eq!(CREATE, 512);
        assert_eq!(PS, 1024);
        assert_eq!(PANEL, 2048);
        assert_eq!(ARRIBA, 4096);
        assert_eq!(ABAJO, 8192);
        assert_eq!(IZQUIERDA, 16384);
        assert_eq!(DERECHA, 32768);
    }

    #[test]
    fn suelto_es_todo_cero() {
        let s = EstadoMando::suelto();
        assert_eq!((s.botones, s.l2, s.r2, s.lx, s.ly, s.rx, s.ry), (0, 0, 0, 0, 0, 0, 0));
    }
}
