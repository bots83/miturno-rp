//! Guardián de latidos: si el cliente deja de latir, hay que soltar todo.

#[derive(Debug, Clone)]
pub struct Guardian {
    timeout_ms: u64,
    ultimo_latido: Option<u64>,
}

impl Guardian {
    pub fn new(timeout_ms: u64) -> Self {
        Self { timeout_ms, ultimo_latido: None }
    }

    pub fn latido(&mut self, ahora_ms: u64) {
        self.ultimo_latido = Some(ahora_ms);
    }

    /// `true` si nunca hubo latido o pasaron `timeout_ms` o más desde el último.
    pub fn vencido(&self, ahora_ms: u64) -> bool {
        match self.ultimo_latido {
            None => true,
            Some(t) => ahora_ms.saturating_sub(t) >= self.timeout_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antes_del_primer_latido_esta_vencido() {
        assert!(Guardian::new(500).vencido(0));
    }

    #[test]
    fn vence_a_los_500_ms_sin_latido() {
        let mut g = Guardian::new(500);
        g.latido(1_000);
        assert!(!g.vencido(1_000));
        assert!(!g.vencido(1_499));
        assert!(g.vencido(1_500));
    }

    #[test]
    fn un_latido_lo_renueva() {
        let mut g = Guardian::new(500);
        g.latido(0);
        g.latido(400);
        assert!(!g.vencido(899));
        assert!(g.vencido(900));
    }
}
